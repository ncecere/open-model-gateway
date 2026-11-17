//! Gateway-run batches (0021): each line executes through the normal
//! inference engine (routing, adapters, deadlines, explicit failover policy)
//! with its own durable execution and reservation, transferred out of the
//! batch's ceiling (`governance::batch::admit_line`).
//!
//! - **Workers:** one runner per process claims batches with a renewable
//!   lease (a crashed runner's batches resume elsewhere). Lines run on a
//!   bounded pool (`GATEWAY_BATCH_WORKERS`, default 4) with a per-batch cap
//!   (`GATEWAY_BATCH_CONCURRENCY`, default 2).
//! - **Claim fence (0081, [`super::fence`]):** each claim bumps the batch's
//!   epoch; every write of a run checks it in its own transaction, so a run
//!   that lost the batch (a stall past the lease, failed renewals) writes
//!   nothing more and stops.
//! - **Scheduling (0022, [`super::schedule`]):** a line starts only when its
//!   route has capacity: the route's window, live traffic and server load
//!   gates are open and a fair-share claim under the route's concurrency
//!   succeeds. Each line is pinned to the route it was scheduled on (no
//!   failover after dispatch). Lines of different models (routes) progress
//!   independently; within a batch, models take turns.
//! - **Exactly once:** a line is claimed by inserting its `batch_lines` row
//!   before it runs. A line a crashed runner left `running` becomes
//!   `interrupted` and is never executed again (its result is reported as
//!   unavailable; its hold is reconciled as unknown).
//! - **No implicit retries:** a failed line is recorded failed. A batch may
//!   opt into up to two retries of retryable failures (`omg_retries`), each a
//!   new attempt with its own reservation. Upstream rate limiting (429)
//!   pauses the batch's dispatch with backoff; nothing is resent.
//! - **Stops:** cancel (running lines finish, `cancelled`), the completion
//!   window (24 h by default; `expired`) and budget exhaustion (`failed`, `budget_exceeded`). Lines
//!   that never ran are listed in the error file.
//! - **Results:** finished lines are written to encrypted segments
//!   (internal files) as they complete, then merged into the `batch_output`
//!   output and error files. Bodies are never stored in PostgreSQL.
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gateway_datetime_compat::UtcInstant;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use super::{
    fence::{BatchFence, FenceError, FenceResult},
    io::{LazySink, LineReader, RESULT_LINE_BYTES},
    lines::{self, NotRun},
    schedule::{Claim, Pause, Scheduler, WaitRow},
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

mod dispatch;
mod lanes;
mod line;
mod renewal;
mod repository;
mod segments;
use dispatch::Dispatch;
use lanes::{Head, Lane, Pin, Planned, note, permits, spawn_line, wait_reason_without_permits};
use line::{LineRun, Record, RunState, Stop};
use repository::LineRepository;
#[cfg(test)]
pub(crate) use segments::Probe as SegmentProbe;
use segments::write_segments;

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
/// How long a dispatch pass that started nothing waits before looking again.
const POLL: Duration = Duration::from_millis(500);
/// A waiting line's route is planned again after this long (routes can be
/// disabled or cool down meanwhile).
const REPLAN: Duration = Duration::from_secs(10);
/// Unchanged demand rows are re-published (heartbeat) at least this often.
const HEARTBEAT: Duration = Duration::from_secs(3);

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
    /// Route gates and fair-share claims (capacity-aware scheduling).
    pub(crate) scheduler: Arc<Scheduler>,
    /// Wait between dispatch passes that started nothing.
    pub(crate) poll: Duration,
}

impl Runner {
    pub fn new(jobs: Jobs) -> Self {
        let workers = jobs.limits.batch_workers.max(1);
        crate::metrics::METRICS.set_batch_workers(workers as u64, 0);
        let scheduler = Arc::new(Scheduler::new(jobs.store.clone(), jobs.approvals.clone()));
        Self {
            jobs,
            id: Uuid::new_v4(),
            pool: Arc::new(Semaphore::new(workers)),
            active: Arc::default(),
            scheduler,
            poll: POLL,
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
        // The runs still going keep their claim: a claim of the same batch
        // would end it (the epoch moves on) without starting another run.
        let running: Vec<Uuid> = self
            .active
            .lock()
            .expect("runner set")
            .iter()
            .copied()
            .collect();
        let claimed = store::claim_gateway(
            &self.jobs.store,
            self.id,
            LEASE_SECONDS,
            room as i64,
            &running,
        )
        .await?;
        let started = claimed.len();
        for (job, fence) in claimed {
            if !self.active.lock().expect("runner set").insert(job.id) {
                continue;
            }
            let runner = self.clone();
            tokio::spawn(async move {
                let id = job.id;
                if runner.run(job, fence).await.is_err() {
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
            .await
            .map_err(store::job_storage);
        if let Ok(rows) = rows {
            for mode in ["native", "gateway"] {
                let n = rows.iter().find(|r| r.0 == mode).map_or(0, |r| r.1);
                crate::metrics::METRICS.set_batch_queue(mode, n.max(0) as u64);
            }
        }
        if super::schedule::observe_metrics(&self.jobs.store)
            .await
            .is_err()
        {
            tracing::warn!("batch route gauges not refreshed");
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
        let Some((job, fence)) =
            store::claim_gateway(&self.jobs.store, self.id, LEASE_SECONDS, 1, &[])
                .await?
                .pop()
        else {
            return Ok(false);
        };
        self.run(job, fence).await?;
        Ok(true)
    }

    /// Run a claimed batch: resume, dispatch remaining lines, finalize.
    async fn run(&self, job: JobRow, fence: BatchFence) -> JobResult<()> {
        let files = self.jobs.files()?.clone();
        let engine = self
            .jobs
            .engine
            .clone()
            .ok_or(InferenceError::Configuration)?;
        let endpoint = job.endpoint().ok_or(InferenceError::Storage)?;
        let work = job.work_file_id.ok_or(InferenceError::Storage)?;
        let state = Arc::new(RunState {
            #[cfg(test)]
            pauses: self.jobs.rate_limit_pauses.clone(),
            #[cfg(test)]
            soft_loss: self.jobs.soft_loss.clone(),
            ..RunState::new(fence)
        });
        // Lease renewal (and cancel observation) while the batch runs.
        let renewal = {
            let store = self.jobs.store.clone();
            renewal::spawn(state.clone(), move || {
                let store = store.clone();
                async move { fence.renew(&store, LEASE_SECONDS).await.map_err(|_| ()) }
            })
        };
        let result = self
            .run_claimed(&job, &files, engine, endpoint, work, state.clone())
            .await;
        renewal.abort();
        // A run that failed, or stopped because it doubted its lease, gives
        // the batch up at once instead of making the next runner wait out the
        // term. A refused release (the claim ended) is the normal case of a
        // fenced run.
        if result.is_err()
            || (state.lost.load(Ordering::SeqCst) && !state.fenced.load(Ordering::SeqCst))
        {
            let _ = fence.release(&self.jobs.store).await;
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
            return self.settle_finished(job, files, &state).await;
        }
        let claimed = self.resume(job, &state).await?;
        if state.lost.load(Ordering::SeqCst) {
            return Ok(());
        }
        // No line can be admitted against an envelope that is not pending, so
        // none is attempted: the batch ends failed with that reason instead of
        // every line failing as a provider configuration error. The envelope
        // is left as it is, for reconciliation.
        if !store::reservation_pending(store, job.execution_id).await? {
            tracing::warn!(job_id = %job.id, "batch budget hold unavailable; remaining lines will not run");
            state.hold_unavailable.store(true, Ordering::SeqCst);
        }
        let line = self.line_run(job, engine, endpoint, &state);
        let (records, receiver) = mpsc::channel::<Record>(64);
        let writer = tokio::spawn(write_segments(
            store.clone(),
            files.clone(),
            job.clone(),
            state.clone(),
            receiver,
        ));
        let per_batch = Arc::new(Semaphore::new(self.jobs.limits.batch_concurrency));
        let mut tasks = tokio::task::JoinSet::new();
        let principal = job.principal();
        let run = Dispatch {
            job,
            files,
            work,
            endpoint,
            state: &state,
            principal: &principal,
            per_batch: &per_batch,
            line: &line,
            records: &records,
        };
        // Capacity-aware dispatch: lanes (one per model) take turns; a lane's
        // next line starts only when its route's gates are open and the
        // fair-share claim succeeds. Nothing starts after a stop.
        let dispatched = self.dispatch(&run, &mut tasks, &claimed).await;
        // Running lines always finish (cancel semantics); nothing is dropped.
        while tasks.join_next().await.is_some() {}
        drop(records);
        drop(line);
        let segments = writer.await.map_err(|_| InferenceError::Storage)?;
        dispatched?;
        segments?;
        // Read once. A run that lost the batch (fenced) or only doubts its
        // lease (soft loss) never ends it: it leaves the batch non-terminal,
        // and `run` releases it, so the claim that holds it next finalises.
        // Only a real reason ends a batch: all lines done, cancel, expiry or
        // a budget stop.
        let stop = state.stop_reason(job);
        if stop == Some(Stop::Lost) {
            return Ok(());
        }
        self.finalize(job, files, work, stop, &state).await
    }

    /// Resume a claimed batch: observe a recorded cancel, interrupt (and
    /// count) the lines a crashed runner left running, mark it running and
    /// return the lines already claimed.
    async fn resume(&self, job: &JobRow, state: &RunState) -> JobResult<HashSet<i32>> {
        let store = &self.jobs.store;
        if job.cancel_requested_at.is_some() {
            state.cancel.store(true, Ordering::SeqCst);
        }
        // Resume: lines a crashed or lost runner left running are never
        // re-executed (and are counted failed in the same transaction).
        let fence = &state.fence;
        if state
            .written(fence.interrupt_running(store).await)?
            .is_none()
            || state.written(fence.mark_running(store).await)?.is_none()
        {
            return Ok(HashSet::new());
        }
        let claimed: HashSet<i32> = store::line_states(store, job.id)
            .await?
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        Ok(claimed)
    }

    /// The shared per-line runner: the engine bound to the batch's line
    /// repository (lease, admission and pins).
    fn line_run(
        &self,
        job: &JobRow,
        engine: Engine,
        endpoint: BatchEndpoint,
        state: &Arc<RunState>,
    ) -> Arc<LineRun> {
        let store = &self.jobs.store;
        let repository = Arc::new(LineRepository {
            store: store.clone(),
            batch: LineContext {
                envelope: job.execution_id,
                fence: state.fence,
            },
            pins: Mutex::default(),
            claimed: Mutex::default(),
        });
        Arc::new(LineRun {
            store: store.clone(),
            engine: engine.with_repository(repository.clone(), self.jobs.limits.batch_concurrency),
            repository,
            scheduler: self.scheduler.clone(),
            job: job.clone(),
            endpoint,
            state: state.clone(),
        })
    }

    /// Publish the pass's demand rows when they changed or the last
    /// publication is older than [`HEARTBEAT`].
    async fn publish_demand(
        &self,
        job: &JobRow,
        state: &RunState,
        report: BTreeMap<Uuid, WaitRow>,
        published: &mut Option<(Instant, Vec<WaitRow>)>,
    ) {
        let rows: Vec<WaitRow> = report
            .into_values()
            .filter(|r| r.waiting_lines > 0)
            .collect();
        let due = published
            .as_ref()
            .is_none_or(|(at, last)| *last != rows || at.elapsed() >= HEARTBEAT);
        if due {
            let legit = rows.iter().any(|r| r.reason.is_some_and(Pause::legitimate));
            let published_now = self
                .scheduler
                .publish_waits(&state.fence, job.workspace_id, &rows, legit)
                .await;
            if let Err(e) = published_now {
                match e {
                    FenceError::Stale => state.lose(),
                    FenceError::Storage | FenceError::Unknown => {
                        tracing::warn!(job_id = %job.id, "batch demand not recorded");
                    }
                }
            }
            *published = Some((Instant::now(), rows));
        }
    }

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    async fn finalize(
        &self,
        job: &JobRow,
        files: &crate::filestore::FileStorage,
        work: Uuid,
        stop: Option<Stop>,
        state: &RunState,
    ) -> JobResult<()> {
        let store = &self.jobs.store;
        let fence = &state.fence;
        let final_state = match stop {
            None => JobState::Completed,
            Some(Stop::Cancelled) => JobState::Cancelled,
            Some(Stop::Expired) => JobState::Expired,
            Some(Stop::Budget | Stop::HoldUnavailable) => JobState::Failed,
            // Never a terminal state: see `run_claimed`.
            Some(Stop::Lost) => return Ok(()),
        };
        match self.scheduler.clear_waits(fence).await {
            Ok(()) => {}
            Err(FenceError::Stale) => {
                state.lose();
                return Ok(());
            }
            Err(FenceError::Storage | FenceError::Unknown) => {
                tracing::warn!(job_id = %job.id, "batch demand not cleared; it goes stale");
            }
        }
        if job.cancel_requested_at.is_none()
            && stop != Some(Stop::Cancelled)
            && state
                .written(fence.set_status(store, "finalizing").await)?
                .is_none()
        {
            return Ok(());
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
                Some(Stop::HoldUnavailable) => NotRun::HoldUnavailable,
                Some(Stop::Lost) | None => NotRun::Failed,
            };
            let mut reader = LineReader::stored(files, work, job.workspace_id, WORK_LINE_BYTES)
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
        let error_file = match err.finish().await {
            Ok(file) => file,
            Err(e) => {
                remove_files(files, job, [output_file, None]).await;
                return Err(e.into());
            }
        };
        #[cfg(test)]
        self.jobs.finalize_hold.pause().await;
        // A line recorded `succeeded` whose result was never stored (it sat in
        // the segment buffer of a run that lost the batch) is listed in the
        // error file as unavailable, so it is counted failed, not completed.
        let counted = sqlx::query_as("SELECT count(*) FILTER(WHERE state='succeeded' AND segment IS NOT NULL),count(*) FILTER(WHERE state IN('failed','interrupted') OR (state='succeeded' AND segment IS NULL)) FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&store.pool)
            .await
            .map_err(store::job_storage);
        let (completed, failed): (i64, i64) = match counted {
            Ok(counts) => counts,
            Err(e) => {
                remove_files(files, job, [output_file, error_file]).await;
                return Err(e.into());
            }
        };
        let finished = fence
            .finish_batch(
                store,
                &Finished {
                    state: final_state,
                    error_code: match stop {
                        Some(Stop::Budget) => Some("budget_exceeded"),
                        Some(Stop::HoldUnavailable) => Some("hold_unavailable"),
                        _ => None,
                    },
                    output_file,
                    error_file,
                    counts: job.request_total.map(|total| RequestCounts {
                        total: total.max(0) as u32,
                        completed: completed.max(0) as u32,
                        failed: failed.max(0) as u32,
                    }),
                },
            )
            .await;
        // The result files are removed when the outcome is certain and the
        // batch does not reference them: the claim ended (zero rows), or the
        // statement was rolled back. When the connection failed with the
        // statement possibly sent, the server may still commit it, so the
        // files stay (a later sweep removes them if it did not).
        let job = match finished {
            Ok(row) => {
                let unused = [output_file, error_file].map(|f| {
                    f.filter(|f| row.output_file_id != Some(*f) && row.error_file_id != Some(*f))
                });
                remove_files(files, job, unused).await;
                row
            }
            Err(FenceError::Stale) => {
                state.lose();
                remove_files(files, job, [output_file, error_file]).await;
                return Ok(());
            }
            Err(FenceError::Storage) => {
                remove_files(files, job, [output_file, error_file]).await;
                return Err(InferenceError::Storage.into());
            }
            Err(FenceError::Unknown) => return Err(InferenceError::Storage.into()),
        };
        let _ = segments;
        self.settle_finished(&job, files, state).await?;
        crate::metrics::METRICS.observe_batch_finished("gateway", final_state.as_str());
        Ok(())
    }

    /// Settle a finished batch (this run's claim is still current, checked in
    /// the transaction that closes the envelope): release what no line used
    /// (lines keep their own settlement or retained unknown holds), mark it
    /// settled, then delete the segments and the private copy.
    async fn settle_finished(
        &self,
        job: &JobRow,
        files: &crate::filestore::FileStorage,
        state: &RunState,
    ) -> JobResult<()> {
        use crate::governance::batch::{Closed, close_gateway_batch};
        let store = &self.jobs.store;
        let elapsed = (UtcInstant::now() - job.created_at)
            .whole_milliseconds()
            .max(0) as u64;
        match close_gateway_batch(store, job.execution_id, elapsed, &state.fence).await? {
            Closed::Settled => {}
            Closed::NotPending => {
                tracing::warn!(job_id = %job.id, "batch envelope no longer pending; its hold is left to reconciliation");
            }
            Closed::Fenced => {
                state.lose();
                return Ok(());
            }
        }
        if state
            .written(state.fence.mark_settled(store).await)?
            .is_none()
        {
            return Ok(());
        }
        for (_, file) in store::segments(store, job.id).await? {
            let _ = files.delete(file, Some(job.workspace_id)).await;
        }
        self.jobs.discard_work_file(job).await;
        Ok(())
    }

    /// The route a line would use right now: the engine's own candidate
    /// filter (live catalog and key access, protocol, adapter support,
    /// deterministic per-adapter eligibility for an unpinned line) and
    /// route plan, first candidate.
    async fn plan_route(
        &self,
        principal: &Principal,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution: Uuid,
        planned_deployment: Option<Uuid>,
    ) -> Planned {
        match self
            .plan_route_inner(principal, endpoint, request, execution, planned_deployment)
            .await
        {
            Ok(Some(d)) => Planned::Route(d),
            Err(InferenceError::RouteCoolingDown(_)) => Planned::CoolingDown,
            // A transient planning failure is not "no route serves this line":
            // wait and plan again instead of dispatching it ungated, because an
            // ungated attempt may only be refused at admission (#243), which
            // would fail a line that a moment later had a route.
            Err(InferenceError::Storage | InferenceError::Timeout) => Planned::CoolingDown,
            _ => Planned::Nothing,
        }
    }

    async fn plan_route_inner(
        &self,
        principal: &Principal,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution: Uuid,
        planned_deployment: Option<Uuid>,
    ) -> Result<Option<Deployment>, InferenceError> {
        let protocol = endpoint.protocol();
        let model = request.model();
        let store = &self.jobs.store;
        let candidates: Vec<Deployment> = store
            .deployments(principal, model)
            .await?
            .into_iter()
            .filter(|d| {
                planned_deployment.is_none_or(|id| id == d.id)
                    && d.supported_protocols.iter().any(|p| p == protocol.as_str())
                    && self.jobs.registry.get(&d.provider).is_some_and(|a| {
                        a.supports_protocol(protocol)
                            && match request {
                                BatchRequest::Chat(r) => a.supports_chat_request(r),
                                BatchRequest::Embeddings(r) => a.supports_embedding_target(d, r),
                            }
                    })
            })
            .collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        // An unpinned (explicit-maximum) line is not yet tied to a route:
        // the coarse filter above can pass while an adapter's own pure
        // encoder (`ProviderAdapter::validate_chat_request`, #228) still
        // refuses this exact body (a strict function tool, an out-of-range
        // sampling parameter, ...). Claiming and pinning such a route would
        // then deterministically refuse the line even though another
        // authorized, protocol-capable route accepts it (#243): apply the
        // same deterministic eligibility the engine itself runs before
        // admission (`Engine::execute_protocol`), once per adapter
        // (provider id, not per deployment), and choose only among routes
        // that pass. A pinned (omitted-maximum) line's candidate list is
        // already narrowed to its one route above and is left unfiltered
        // here: it stays judged on that pinned route alone, inside the
        // engine at dispatch, exactly as before (#238).
        let candidates: Vec<Deployment> = if planned_deployment.is_none() {
            if let BatchRequest::Chat(r) = request {
                let mut verdicts: HashMap<String, bool> = HashMap::new();
                let eligible: Vec<Deployment> = candidates
                    .into_iter()
                    .filter(|d| {
                        *verdicts.entry(d.provider.clone()).or_insert_with(|| {
                            self.jobs
                                .registry
                                .get(&d.provider)
                                .is_some_and(|a| a.validate_chat_request(r).is_ok())
                        })
                    })
                    .collect();
                if eligible.is_empty() {
                    // Every authorized, protocol-capable route refuses this
                    // exact body: claim nothing here. The line proceeds
                    // ungated to the engine's normal unpinned refusal path
                    // (`candidate_deployments` returns every deployment),
                    // reported with the engine's own 400/501
                    // classification, not a new error path invented here.
                    return Ok(None);
                }
                eligible
            } else {
                candidates
            }
        } else {
            candidates
        };
        let plan = store
            .route_plan(principal, model, &candidates, execution)
            .await?;
        let Some(first) = plan.deployment_ids.first() else {
            return Ok(None);
        };
        Ok(candidates.into_iter().find(|d| d.id == *first))
    }
}

/// Delete result files nothing references.
async fn remove_files(
    files: &crate::filestore::FileStorage,
    job: &JobRow,
    created: [Option<Uuid>; 2],
) {
    for file in created.into_iter().flatten() {
        let _ = files.delete(file, Some(job.workspace_id)).await;
    }
}

#[cfg(test)]
mod wait_reason_tests {
    use super::*;

    #[test]
    fn a_full_route_is_reported_as_its_batch_limit_not_workers() {
        // Defaults: route limit 2 == per-batch cap 2.
        assert_eq!(wait_reason_without_permits(Some(2), 2), Pause::Concurrency);
        assert_eq!(wait_reason_without_permits(Some(3), 2), Pause::Concurrency);
        assert_eq!(wait_reason_without_permits(Some(1), 4), Pause::Workers);
        // Unknown route load: fall back to the gateway's own limit.
        assert_eq!(wait_reason_without_permits(None, 2), Pause::Workers);
    }
}
