//! Capacity-aware dispatch of a claimed batch's lines: lanes take turns,
//! each head line is planned, gated and claimed before it starts.
use super::*;

/// What one batch's dispatch passes share (borrowed from `run_claimed`).
pub(super) struct Dispatch<'a> {
    pub(super) job: &'a JobRow,
    pub(super) files: &'a crate::filestore::FileStorage,
    pub(super) work: Uuid,
    pub(super) endpoint: BatchEndpoint,
    pub(super) state: &'a RunState,
    pub(super) principal: &'a Principal,
    pub(super) per_batch: &'a Arc<Semaphore>,
    pub(super) line: &'a Arc<LineRun>,
    pub(super) records: &'a mpsc::Sender<Record>,
}

impl Runner {
    /// Dispatch passes until every lane is empty or the batch stops.
    pub(super) async fn dispatch(
        &self,
        run: &Dispatch<'_>,
        tasks: &mut tokio::task::JoinSet<()>,
        claimed: &HashSet<i32>,
    ) -> Result<(), InferenceError> {
        let (job, state) = (run.job, run.state);
        let mut lanes =
            Lane::scan(run.files, run.work, job.workspace_id, run.endpoint, claimed).await?;
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
                started |= self.dispatch_lane(run, tasks, lane, &mut report).await?;
            }
            turn = turn.wrapping_add(1);
            self.publish_demand(job, report, &mut published).await;
            if !started {
                self.idle(tasks).await;
            }
        }
        Ok(())
    }

    /// One lane's turn in a dispatch pass: start its head line if its route
    /// has capacity, otherwise note why it waits. `true` when a line started.
    async fn dispatch_lane(
        &self,
        run: &Dispatch<'_>,
        tasks: &mut tokio::task::JoinSet<()>,
        lane: &mut Lane,
        report: &mut BTreeMap<Uuid, WaitRow>,
    ) -> Result<bool, InferenceError> {
        let job = run.job;
        lane.load(run.files, run.work, job.workspace_id, run.endpoint)
            .await?;
        let waiting = lane.lines.len() as i32;
        let Some(head) = lane.head.as_mut() else {
            return Ok(false);
        };
        self.plan_head(run, head).await;
        if head.cooling {
            // Every route is in its circuit-breaker cooldown: the
            // line waits (nothing is sent) and is planned again,
            // instead of failing like an interactive request.
            return Ok(false);
        }
        let (n, execution) = (head.n, head.execution);
        let route = head.route.as_ref().map(|d| d.id);
        let Some(route) = route else {
            // No route serves the line right now: it runs ungated
            // and fails in the engine exactly like an interactive
            // request would (nothing reaches a provider).
            return self.start_ungated(run, tasks, lane, n, execution).await;
        };
        if run.state.paused() {
            note(report, route, waiting, Some(Pause::RateLimited), false);
            return Ok(false);
        }
        let gate = match self.scheduler.gate(route).await {
            Ok(g) => g,
            Err(_) => {
                note(report, route, waiting, None, false);
                return Ok(false);
            }
        };
        if let Some(pause) = gate.pause {
            note(report, route, waiting, Some(pause), false);
            return Ok(false);
        }
        let settings = &gate.settings.settings;
        let Some(permits) = permits(run.per_batch, &self.pool) else {
            // Report the binding limit: with the defaults the
            // route's batch limit equals the per-batch cap, so a
            // full route must not read as "workers busy".
            let running = self.scheduler.running_lines(route).await.ok();
            let reason = wait_reason_without_permits(running, settings.max_concurrency);
            note(
                report,
                route,
                waiting,
                Some(reason),
                reason == Pause::Concurrency,
            );
            return Ok(false);
        };
        let mut started = false;
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
                spawn_line(tasks, run.line, run.records, head, Some(pin), permits);
                started = true;
                note(report, route, waiting - 1, None, true);
            }
            Ok(Claim::Taken) => {
                // Claimed elsewhere: never run it again.
                lane.take();
            }
            Ok(Claim::Full) => note(report, route, waiting, Some(Pause::Concurrency), true),
            Ok(Claim::Wait) => note(report, route, waiting, Some(Pause::FairShare), true),
            Err(_) => note(report, route, waiting, None, false),
        }
        Ok(started)
    }

    /// Plan the head line's route again when it was never planned or its
    /// plan is older than [`REPLAN`].
    async fn plan_head(&self, run: &Dispatch<'_>, head: &mut Head) {
        if head.planned.is_none_or(|t| t.elapsed() >= REPLAN) {
            (head.route, head.cooling) = match &head.request {
                Some(request) => {
                    match self
                        .plan_route(
                            run.principal,
                            run.endpoint,
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
    }

    /// Start a line no route serves, ungated, once it is claimed. `true` when
    /// it started.
    async fn start_ungated(
        &self,
        run: &Dispatch<'_>,
        tasks: &mut tokio::task::JoinSet<()>,
        lane: &mut Lane,
        n: i32,
        execution: Uuid,
    ) -> Result<bool, InferenceError> {
        let job = run.job;
        let Some(permits) = permits(run.per_batch, &self.pool) else {
            return Ok(false);
        };
        let mut started = false;
        match store::claim_line(&self.jobs.store, job.id, job.workspace_id, n, execution).await {
            Ok(true) => {
                let head = lane.take().ok_or(InferenceError::Storage)?;
                spawn_line(tasks, run.line, run.records, head, None, permits);
                started = true;
            }
            Ok(false) => {
                lane.take();
            }
            Err(_) => {}
        }
        Ok(started)
    }

    /// After a pass that started nothing: wait for a running line to finish
    /// or the poll interval, whichever comes first.
    async fn idle(&self, tasks: &mut tokio::task::JoinSet<()>) {
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
