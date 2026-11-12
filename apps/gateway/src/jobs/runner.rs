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

mod lanes;
mod line;
mod repository;
mod segments;
use lanes::{Lane, Pin, Planned, note, permits, spawn_line, wait_reason_without_permits};
use line::{LineRun, Record, RunState, Stop};
use repository::LineRepository;
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
        let state = Arc::new(RunState {
            #[cfg(test)]
            pauses: self.jobs.rate_limit_pauses.clone(),
            ..RunState::default()
        });
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

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
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
            claimed: Mutex::default(),
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
                                    .plan_route(
                                        &principal,
                                        endpoint,
                                        request,
                                        head.execution,
                                        head.plan.map(|p| p.deployment),
                                    )
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

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
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
