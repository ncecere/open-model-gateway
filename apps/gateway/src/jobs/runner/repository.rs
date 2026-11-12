//! The inference repository of a batch's line attempts: admission transfers
//! each line's hold out of the batch's ceiling.
use super::*;

/// Repository of a gateway-run batch's line attempts: everything is the
/// normal store, except that admission transfers each line's hold out of
/// the batch's ceiling (`governance::batch::admit_line`).
///
/// `pins` carries each attempt's routing/pricing plan: an omitted-maximum
/// line's creation-time plan (`lines::LinePlan`), or an explicit-maximum
/// line's dispatch-time route, used for `candidate_deployments`/
/// `route_plan` and (when admitted) the price, exactly as before (#215,
/// #238) - a line that carries a plan is still judged against that one
/// route, never every candidate.
///
/// `claimed` is a separate, narrower fact (#243 review finding 1): whether
/// THIS attempt actually holds the scheduler's route-capacity claim
/// (`Runner`'s dispatch passed it a `Pin`). A plan alone is not a claim -
/// an omitted-maximum line whose pinned route was briefly ineligible at
/// dispatch still spawns ungated (no `Pin`) even though `pins` still
/// records its plan for routing/pricing. `admit`/`admit_embeddings`/
/// `admit_workload` refuse every attempt that is not in `claimed` (#243):
/// an ungated line may only be refused, never admitted, even if the live
/// catalog or an adapter's deterministic eligibility changes in its favour
/// between dispatch and here.
pub(super) struct LineRepository {
    pub(super) store: Store,
    pub(super) batch: LineContext,
    /// Attempt (execution id) → its routing/pricing plan (creation-time for
    /// an omitted maximum, dispatch-time route for an explicit maximum).
    pub(super) pins: Mutex<HashMap<Uuid, (Uuid, Option<Uuid>)>>,
    /// Attempts the scheduler actually claimed route capacity for at
    /// dispatch (a `Pin` was passed to `LineRun::execute`), distinct from
    /// merely carrying a plan in `pins`.
    pub(super) claimed: Mutex<HashSet<Uuid>>,
}
impl LineRepository {
    /// Record `execution`'s routing/pricing plan. `claimed` is `true` only
    /// when the scheduler itself claimed route capacity for this attempt at
    /// dispatch (`pin.is_some()` at the `LineRun::execute` call site), never
    /// merely because a creation-time plan exists.
    pub(super) fn pin(
        &self,
        execution: Uuid,
        deployment: Uuid,
        price: Option<Uuid>,
        claimed: bool,
    ) {
        self.pins
            .lock()
            .expect("pins")
            .insert(execution, (deployment, price));
        if claimed {
            self.claimed.lock().expect("claimed").insert(execution);
        }
    }
    pub(super) fn unpin(&self, execution: Uuid) {
        self.pins.lock().expect("pins").remove(&execution);
        self.claimed.lock().expect("claimed").remove(&execution);
    }
    /// Whether the scheduler actually claimed route capacity for `execution`
    /// at dispatch - never merely whether it carries a routing/pricing plan
    /// (#243 review finding 1: those are different facts kept in `pins` and
    /// `claimed`).
    fn gated(&self, execution: Uuid) -> bool {
        self.claimed.lock().expect("claimed").contains(&execution)
    }
    /// An ungated attempt reaching admission (#243): the runner's dispatch
    /// decision (`Runner::plan_route_inner`) found no route to claim for
    /// this line - every candidate deterministically rejected it, or none
    /// existed - so it proceeded expecting only the engine's own refusal.
    /// `candidate_deployments` below still returns every live deployment
    /// for it, unfiltered, so that refusal keeps its correct classification
    /// when truly nothing serves the line. But the live catalog and an
    /// adapter's deterministic eligibility can change before admission: if
    /// some candidate now accepts, admitting it here would execute the line
    /// on a route the scheduler never claimed capacity on, bypassing route
    /// windows, live-load gates, fair-share ordering and cross-runner route
    /// concurrency accounting (`batch_lines.deployment_id` stays NULL for
    /// it). An ungated line may only be refused, never admitted: refuse
    /// with an existing, already-classified, already-retryable denial (not
    /// a new error path) - the batch's own explicit retry policy, if any,
    /// gets its own bounded additional attempts, each independently and
    /// correctly refused the same way while still ungated.
    fn refuse_ungated(&self, record: &ExecutionStart, deployment: &Deployment) -> InferenceError {
        tracing::warn!(
            execution_id = %record.id,
            deployment_id = %deployment.id,
            "batch line admission refused: ungated attempt found an admissible route after dispatch (#243)"
        );
        InferenceError::Busy
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
    /// Every deployment of the model, unless this attempt is pinned to one
    /// route (an omitted-maximum line pinned at plan time, or an explicit-
    /// maximum line the scheduler already claimed capacity on): then only
    /// that route, so the engine's deterministic pre-admission check and
    /// its refusal classification judge the pin on its own route, matching
    /// `route_plan`'s existing pin enforcement below (#238).
    async fn candidate_deployments(
        &self,
        principal: &Principal,
        model: &str,
        request_id: Uuid,
    ) -> Result<Vec<Deployment>, InferenceError> {
        let deployments = self.store.deployments(principal, model).await?;
        let pinned = self
            .pins
            .lock()
            .expect("pins")
            .get(&request_id)
            .map(|p| p.0);
        Ok(match pinned {
            Some(d) => deployments.into_iter().filter(|dep| dep.id == d).collect(),
            None => deployments,
        })
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
        output_limit: u32,
    ) -> Result<crate::inference::repository::GenerationAdmission, InferenceError> {
        if !self.gated(record.id) {
            return Err(self.refuse_ungated(record, deployment));
        }
        let price = self
            .pins
            .lock()
            .expect("pins")
            .get(&record.id)
            .and_then(|p| p.1);
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
            price,
            output_limit,
            Some(request),
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
        if !self.gated(record.id) {
            return Err(self.refuse_ungated(record, deployment));
        }
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
            None,
            u32::MAX,
            None,
        )
        .await
        .map(|_| ())
    }
    async fn admit_workload(
        &self,
        record: &ExecutionStart,
        admission: &WorkloadAdmission,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        if !self.gated(record.id) {
            return Err(self.refuse_ungated(record, deployment));
        }
        crate::governance::batch::admit_line(
            &self.store,
            self.batch,
            record,
            admission,
            lease_seconds,
            deployment,
            None,
            u32::MAX,
            None,
        )
        .await
        .map(|_| ())
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
        let pinned = self
            .pins
            .lock()
            .expect("pins")
            .get(&request_id)
            .map(|p| p.0);
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
