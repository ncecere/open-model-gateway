//! One claimed line's execution: why a batch stops, its shared run state,
//! and running a line (with its explicit retries) through the engine.
use super::*;

/// Why a batch stopped dispatching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    Cancelled,
    Expired,
    Budget,
    /// Another runner took the lease.
    Lost,
}

#[derive(Default)]
pub(super) struct RunState {
    pub(super) cancel: AtomicBool,
    pub(super) lost: AtomicBool,
    pub(super) budget: AtomicBool,
    pub(super) busy_streak: AtomicU32,
    pub(super) pause_until: Mutex<Option<tokio::time::Instant>>,
    #[cfg(test)]
    pub(super) pauses: Arc<std::sync::atomic::AtomicUsize>,
}
impl RunState {
    pub(super) fn stop_reason(&self, job: &JobRow) -> Option<Stop> {
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
    pub(super) fn should_stop(&self, job: &JobRow) -> bool {
        self.stop_reason(job).is_some()
    }
    /// Provider rate limiting: back off this batch's dispatch (1 s doubling
    /// to 60 s); a success resets it.
    fn rate_limited(&self) {
        #[cfg(test)]
        self.pauses.fetch_add(1, Ordering::SeqCst);
        let streak = self.busy_streak.fetch_add(1, Ordering::SeqCst).min(6);
        let wait = Duration::from_secs(1u64 << streak).min(Duration::from_secs(60));
        *self.pause_until.lock().expect("pause") = Some(tokio::time::Instant::now() + wait);
    }
    fn succeeded(&self) {
        self.busy_streak.store(0, Ordering::SeqCst);
    }
    /// Whether the batch is backing off after provider rate limiting.
    pub(super) fn paused(&self) -> bool {
        self.pause_until
            .lock()
            .expect("pause")
            .is_some_and(|until| tokio::time::Instant::now() < until)
    }
}

/// One finished line for the segment writer.
pub(super) struct Record {
    pub(super) line: i32,
    pub(super) error: bool,
    pub(super) value: Value,
}

pub(super) struct LineRun {
    pub(super) store: Store,
    pub(super) engine: Engine,
    pub(super) repository: Arc<LineRepository>,
    pub(super) scheduler: Arc<Scheduler>,
    pub(super) job: JobRow,
    pub(super) endpoint: BatchEndpoint,
    pub(super) state: Arc<RunState>,
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
    pub(super) async fn run(
        &self,
        n: i32,
        raw: Vec<u8>,
        mut execution: Uuid,
        pin: Option<Pin>,
        records: mpsc::Sender<Record>,
    ) {
        let line = match lines::parse_work_line(&raw, self.endpoint) {
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
            let result = self
                .execute(line.request.clone(), execution, pin, line.plan)
                .await;
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
                    // Busy from an ungated refusal (`LineRepository::refuse_ungated`,
                    // #243 review finding 2) never reached a provider: only a
                    // claimed attempt's Busy is upstream rate limiting, worth
                    // backing off the whole batch's dispatch for.
                    if e == InferenceError::Busy && pin.is_some() {
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
                        let next = Uuid::now_v7();
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
        mut request: BatchRequest,
        execution: Uuid,
        pin: Option<Pin>,
        plan: Option<lines::LinePlan>,
    ) -> Result<BatchResponse, InferenceError> {
        if let Some(deployment) = plan.map(|p| p.deployment).or(pin.map(|p| p.deployment)) {
            // `pin.is_some()` is the scheduler's own route-capacity claim
            // for this attempt (#243 review finding 1), distinct from
            // merely carrying a plan: a creation-time plan alone (an
            // omitted-maximum line dispatched ungated because its pinned
            // route was briefly ineligible) must not count as gated.
            self.repository
                .pin(execution, deployment, plan.map(|p| p.price), pin.is_some());
        }
        // An omitted-maximum line is pinned at plan time to this one price,
        // which already resolved and validated the line against this exact
        // bound (`jobs::plan::scan`). Copy it onto the still-omitted request
        // before the engine ever sees it, so its pre-admission
        // `validate_chat_request` (and the real upstream attempt) judge the
        // same body planning did, not the raw client omission (#245). `0`
        // only for a plan written before this field existed; the omission
        // is left as-is for those already-queued lines, matching the prior
        // behavior, and admission still resolves the same price-derived
        // bound on its own (#215: the admission-pinned cap equals the
        // upstream cap either way).
        if let (Some(plan), BatchRequest::Chat(r)) = (plan, &mut request)
            && r.max_output_tokens.is_none()
            && plan.output > 0
        {
            r.max_output_tokens = Some(plan.output);
        }
        let attempt = self.execute_attempt(request, execution);
        let result = match pin.and_then(|p| p.priority.map(|priority| (p.deployment, priority))) {
            Some((deployment, priority)) => {
                crate::inference::scheduling::with_priority(deployment, priority, attempt).await
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
