//! Gateway-run batches (0021): each line executes through the normal
//! inference engine (routing, adapters, deadlines, explicit failover policy)
//! with its own durable execution and reservation, transferred out of the
//! batch's ceiling (`governance::batch::admit_line`).
//!
//! - **Workers:** one runner per process claims batches with a renewable
//!   lease (a crashed runner's batches resume elsewhere). Lines run on a
//!   bounded pool (`GATEWAY_BATCH_WORKERS`, default 4) with a per-batch cap
//!   (`GATEWAY_BATCH_CONCURRENCY`, default 2).
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
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use super::{
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
            pins: Mutex::default(),
        });
        let line = Arc::new(LineRun {
            store: store.clone(),
            engine: engine.with_repository(repository.clone(), self.jobs.limits.batch_concurrency),
            repository,
            scheduler: self.scheduler.clone(),
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
        let principal = job.principal();
        // Capacity-aware dispatch: lanes (one per model) take turns; a lane's
        // next line starts only when its route's gates are open and the
        // fair-share claim succeeds. Nothing starts after a stop.
        let dispatched = async {
            let mut lanes = Lane::scan(files, work, job.workspace_id, endpoint, &claimed).await?;
            let mut published: Option<(Instant, Vec<WaitRow>)> = None;
            let mut turn = 0usize;
            loop {
                while tasks.try_join_next().is_some() {}
                lanes.retain(|l| !l.lines.is_empty());
                if lanes.is_empty() || state.should_stop(job) {
                    break;
                }
                let mut started = false;
                let mut report: BTreeMap<Uuid, WaitRow> = BTreeMap::new();
                let count = lanes.len();
                for k in 0..count {
                    if state.should_stop(job) {
                        break;
                    }
                    let lane = &mut lanes[(turn + k) % count];
                    lane.load(files, work, job.workspace_id, endpoint).await?;
                    let waiting = lane.lines.len() as i32;
                    let Some(head) = lane.head.as_mut() else {
                        continue;
                    };
                    if head.planned.is_none_or(|t| t.elapsed() >= REPLAN) {
                        (head.route, head.cooling) = match &head.request {
                            Some(request) => {
                                match self
                                    .plan_route(&principal, endpoint, request, head.execution)
                                    .await
                                {
                                    Planned::Route(d) => (Some(d), false),
                                    Planned::CoolingDown => (None, true),
                                    Planned::Nothing => (None, false),
                                }
                            }
                            None => (None, false),
                        };
                        head.planned = Some(Instant::now());
                    }
                    if head.cooling {
                        // Every route is in its circuit-breaker cooldown: the
                        // line waits (nothing is sent) and is planned again,
                        // instead of failing like an interactive request.
                        continue;
                    }
                    let (n, execution) = (head.n, head.execution);
                    let route = head.route.as_ref().map(|d| d.id);
                    let Some(route) = route else {
                        // No route serves the line right now: it runs ungated
                        // and fails in the engine exactly like an interactive
                        // request would (nothing reaches a provider).
                        let Some(permits) = permits(&per_batch, &self.pool) else {
                            continue;
                        };
                        match store::claim_line(store, job.id, job.workspace_id, n, execution).await
                        {
                            Ok(true) => {
                                let head = lane.take().ok_or(InferenceError::Storage)?;
                                spawn_line(&mut tasks, &line, &records, head, None, permits);
                                started = true;
                            }
                            Ok(false) => {
                                lane.take();
                            }
                            Err(_) => {}
                        }
                        continue;
                    };
                    if state.paused() {
                        note(&mut report, route, waiting, Some(Pause::RateLimited), false);
                        continue;
                    }
                    let gate = match self.scheduler.gate(route).await {
                        Ok(g) => g,
                        Err(_) => {
                            note(&mut report, route, waiting, None, false);
                            continue;
                        }
                    };
                    if let Some(pause) = gate.pause {
                        note(&mut report, route, waiting, Some(pause), false);
                        continue;
                    }
                    let settings = &gate.settings.settings;
                    let Some(permits) = permits(&per_batch, &self.pool) else {
                        // Report the binding limit: with the defaults the
                        // route's batch limit equals the per-batch cap, so a
                        // full route must not read as "workers busy".
                        let running = self.scheduler.running_lines(route).await.ok();
                        let reason = wait_reason_without_permits(running, settings.max_concurrency);
                        note(
                            &mut report,
                            route,
                            waiting,
                            Some(reason),
                            reason == Pause::Concurrency,
                        );
                        continue;
                    };
                    match self
                        .scheduler
                        .claim(job, n, execution, route, settings.max_concurrency)
                        .await
                    {
                        Ok(Claim::Claimed) => {
                            let head = lane.take().ok_or(InferenceError::Storage)?;
                            let pin = Pin {
                                deployment: route,
                                priority: settings.priority,
                                max_concurrency: settings.max_concurrency,
                            };
                            spawn_line(&mut tasks, &line, &records, head, Some(pin), permits);
                            started = true;
                            note(&mut report, route, waiting - 1, None, true);
                        }
                        Ok(Claim::Taken) => {
                            // Claimed elsewhere: never run it again.
                            lane.take();
                        }
                        Ok(Claim::Full) => {
                            note(&mut report, route, waiting, Some(Pause::Concurrency), true)
                        }
                        Ok(Claim::Wait) => {
                            note(&mut report, route, waiting, Some(Pause::FairShare), true)
                        }
                        Err(_) => note(&mut report, route, waiting, None, false),
                    }
                }
                turn = turn.wrapping_add(1);
                let rows: Vec<WaitRow> = report
                    .into_values()
                    .filter(|r| r.waiting_lines > 0)
                    .collect();
                let due = published
                    .as_ref()
                    .is_none_or(|(at, last)| *last != rows || at.elapsed() >= HEARTBEAT);
                if due {
                    let legit = rows.iter().any(|r| r.reason.is_some_and(Pause::legitimate));
                    if self
                        .scheduler
                        .publish_waits(job, &rows, legit)
                        .await
                        .is_err()
                    {
                        tracing::warn!(job_id = %job.id, "batch demand not recorded");
                    }
                    published = Some((Instant::now(), rows));
                }
                if !started {
                    if tasks.is_empty() {
                        tokio::time::sleep(self.poll).await;
                    } else {
                        tokio::select! {
                            _ = tasks.join_next() => {}
                            _ = tokio::time::sleep(self.poll) => {}
                        }
                    }
                }
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
        if self.scheduler.clear_waits(job.id).await.is_err() {
            tracing::warn!(job_id = %job.id, "batch demand not cleared; it goes stale");
        }
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

    /// The route a line would use right now: the engine's own candidate
    /// filter (live catalog and key access, protocol, adapter support) and
    /// route plan, first candidate.
    async fn plan_route(
        &self,
        principal: &Principal,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution: Uuid,
    ) -> Planned {
        match self
            .plan_route_inner(principal, endpoint, request, execution)
            .await
        {
            Ok(Some(d)) => Planned::Route(d),
            Err(InferenceError::RouteCoolingDown(_)) => Planned::CoolingDown,
            _ => Planned::Nothing,
        }
    }

    async fn plan_route_inner(
        &self,
        principal: &Principal,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution: Uuid,
    ) -> Result<Option<Deployment>, InferenceError> {
        let protocol = endpoint.protocol();
        let model = request.model();
        let store = &self.jobs.store;
        let candidates: Vec<Deployment> = store
            .deployments(principal, model)
            .await?
            .into_iter()
            .filter(|d| {
                d.supported_protocols.iter().any(|p| p == protocol.as_str())
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
        let plan = store
            .route_plan(principal, model, &candidates, execution)
            .await?;
        let Some(first) = plan.deployment_ids.first() else {
            return Ok(None);
        };
        Ok(candidates.into_iter().find(|d| d.id == *first))
    }
}

/// The route a claimed line is pinned to.
#[derive(Clone, Copy, Debug)]
struct Pin {
    deployment: Uuid,
    /// vLLM `priority` hint (route setting), sent on this line only.
    priority: Option<i32>,
    max_concurrency: i32,
}

/// Outcome of planning a waiting line's route.
enum Planned {
    Route(Deployment),
    /// Every route serving the line is cooling down: wait, do not fail.
    CoolingDown,
    /// Nothing serves the line (it fails like an interactive request).
    Nothing,
}

/// The next line of a lane, read and planned.
struct Head {
    n: i32,
    raw: Vec<u8>,
    execution: Uuid,
    request: Option<BatchRequest>,
    route: Option<Deployment>,
    /// Planned while every route was cooling down.
    cooling: bool,
    planned: Option<Instant>,
}

/// The unclaimed lines of one model, read in order from the batch's copy.
struct Lane {
    lines: VecDeque<i32>,
    reader: Option<LineReader>,
    read: i32,
    head: Option<Head>,
}
impl Lane {
    /// One pass over the batch's copy: unclaimed line numbers per model
    /// (metadata only; lines are read again when they are due).
    async fn scan(
        files: &crate::filestore::FileStorage,
        work: Uuid,
        workspace: Uuid,
        endpoint: BatchEndpoint,
        claimed: &HashSet<i32>,
    ) -> Result<Vec<Lane>, InferenceError> {
        let mut reader = LineReader::stored(files, work, workspace, BATCH_MAX_LINE_BYTES)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let mut lanes: Vec<Lane> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut n: i32 = -1;
        while let Some(raw) = reader.next_request().await? {
            n += 1;
            if claimed.contains(&n) {
                continue;
            }
            let model = lines::parse_line(&raw, endpoint)
                .map(|l| l.request.model().to_owned())
                .unwrap_or_default();
            let i = *index.entry(model).or_insert_with(|| {
                lanes.push(Lane {
                    lines: VecDeque::new(),
                    reader: None,
                    read: -1,
                    head: None,
                });
                lanes.len() - 1
            });
            lanes[i].lines.push_back(n);
        }
        Ok(lanes)
    }
    /// Read (and parse) the lane's next line if it is not loaded yet.
    async fn load(
        &mut self,
        files: &crate::filestore::FileStorage,
        work: Uuid,
        workspace: Uuid,
        endpoint: BatchEndpoint,
    ) -> Result<(), InferenceError> {
        if self.head.is_some() {
            return Ok(());
        }
        let Some(&n) = self.lines.front() else {
            return Ok(());
        };
        if self.reader.is_none() {
            self.reader = Some(
                LineReader::stored(files, work, workspace, BATCH_MAX_LINE_BYTES)
                    .await
                    .map_err(|_| InferenceError::Storage)?,
            );
            self.read = -1;
        }
        let reader = self.reader.as_mut().ok_or(InferenceError::Storage)?;
        while self.read < n {
            let raw = reader
                .next_request()
                .await?
                .ok_or(InferenceError::Storage)?;
            self.read += 1;
            if self.read == n {
                let request = lines::parse_line(&raw, endpoint).ok().map(|l| l.request);
                self.head = Some(Head {
                    n,
                    raw,
                    execution: Uuid::new_v4(),
                    request,
                    route: None,
                    cooling: false,
                    planned: None,
                });
                return Ok(());
            }
        }
        Err(InferenceError::Storage)
    }
    /// Remove the head (claimed, or claimed elsewhere).
    fn take(&mut self) -> Option<Head> {
        self.lines.pop_front();
        self.head.take()
    }
}

/// This batch's and this process's worker permits, if both are free.
fn permits(
    per_batch: &Arc<Semaphore>,
    pool: &Arc<Semaphore>,
) -> Option<(
    tokio::sync::OwnedSemaphorePermit,
    tokio::sync::OwnedSemaphorePermit,
)> {
    let local = per_batch.clone().try_acquire_owned().ok()?;
    let global = pool.clone().try_acquire_owned().ok()?;
    Some((local, global))
}

/// Why a line waits when no worker permit is free: the route's own batch
/// limit when it is reached (all batches), otherwise the gateway's workers.
fn wait_reason_without_permits(route_running: Option<i64>, max_concurrency: i32) -> Pause {
    match route_running {
        Some(n) if n >= i64::from(max_concurrency) => Pause::Concurrency,
        _ => Pause::Workers,
    }
}

/// Merge one lane's state into the batch's demand on a route.
fn note(
    report: &mut BTreeMap<Uuid, WaitRow>,
    deployment: Uuid,
    waiting: i32,
    reason: Option<Pause>,
    ready: bool,
) {
    let row = report.entry(deployment).or_insert(WaitRow {
        deployment,
        waiting_lines: 0,
        reason,
        ready: false,
    });
    row.waiting_lines = row.waiting_lines.saturating_add(waiting.max(0));
    // A route this batch is dispatching to shows no reason.
    if row.reason.is_some() {
        row.reason = reason;
    }
    row.ready |= ready;
}

fn spawn_line(
    tasks: &mut tokio::task::JoinSet<()>,
    line: &Arc<LineRun>,
    records: &mpsc::Sender<Record>,
    head: Head,
    pin: Option<Pin>,
    permits: (
        tokio::sync::OwnedSemaphorePermit,
        tokio::sync::OwnedSemaphorePermit,
    ),
) {
    let (line, records) = (line.clone(), records.clone());
    tasks.spawn(async move {
        let _permits = permits;
        line.run(head.n, head.raw, head.execution, pin, records)
            .await;
    });
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
        } else if Utc::now() >= job.created_at + job.completion_window() {
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
    /// Whether the batch is backing off after provider rate limiting.
    fn paused(&self) -> bool {
        self.pause_until
            .lock()
            .expect("pause")
            .is_some_and(|until| tokio::time::Instant::now() < until)
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
    repository: Arc<LineRepository>,
    scheduler: Arc<Scheduler>,
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

    /// Run a claimed line (its `batch_lines` row exists) on its pinned route.
    async fn run(
        &self,
        n: i32,
        raw: Vec<u8>,
        mut execution: Uuid,
        pin: Option<Pin>,
        records: mpsc::Sender<Record>,
    ) {
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
            let result = self.execute(line.request.clone(), execution, pin).await;
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
                        if self.claim_retry(n, attempt, next, pin).await {
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

    /// Claim an explicit retry: on its route, within the route's concurrency
    /// (waiting for room; a stop leaves the failure standing).
    async fn claim_retry(&self, n: i32, attempts: i16, execution: Uuid, pin: Option<Pin>) -> bool {
        let Some(pin) = pin else {
            return matches!(
                store::retry_line(&self.store, self.job.id, n, attempts, execution).await,
                Ok(true)
            );
        };
        loop {
            match self
                .scheduler
                .claim_retry(
                    self.job.id,
                    n,
                    attempts,
                    execution,
                    pin.deployment,
                    pin.max_concurrency,
                )
                .await
            {
                Ok(Some(claimed)) => return claimed,
                Ok(None) if !self.state.should_stop(&self.job) => {
                    tokio::time::sleep(POLL).await;
                }
                _ => return false,
            }
        }
    }

    /// Execute one attempt pinned to its route, with the route's priority
    /// hint (if any) scoped to this attempt.
    async fn execute(
        &self,
        request: BatchRequest,
        execution: Uuid,
        pin: Option<Pin>,
    ) -> Result<BatchResponse, InferenceError> {
        let Some(pin) = pin else {
            return self.execute_attempt(request, execution).await;
        };
        self.repository.pin(execution, pin.deployment);
        let attempt = self.execute_attempt(request, execution);
        let result = match pin.priority {
            Some(priority) => {
                crate::inference::scheduling::with_priority(pin.deployment, priority, attempt).await
            }
            None => attempt.await,
        };
        self.repository.unpin(execution);
        result
    }

    async fn execute_attempt(
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
    /// Attempt (execution id) → the route the scheduler claimed it on.
    pins: Mutex<HashMap<Uuid, Uuid>>,
}
impl LineRepository {
    fn pin(&self, execution: Uuid, deployment: Uuid) {
        self.pins
            .lock()
            .expect("pins")
            .insert(execution, deployment);
    }
    fn unpin(&self, execution: Uuid) {
        self.pins.lock().expect("pins").remove(&execution);
    }
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
        // A scheduled line runs only on the route its capacity was claimed
        // on (one attempt, no failover); if that route no longer serves it,
        // the line fails like an interactive request to a missing model.
        let pinned = self.pins.lock().expect("pins").get(&request_id).copied();
        match pinned {
            Some(d) if candidates.iter().any(|c| c.id == d) => {
                Ok(crate::routing::RoutePlan::single(vec![d]))
            }
            Some(_) => Err(InferenceError::ModelUnavailable),
            None => {
                self.store
                    .route_plan(principal, model, candidates, request_id)
                    .await
            }
        }
    }
    async fn route_result(
        &self,
        deployment: Uuid,
        error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        self.store.route_result(deployment, error).await
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
