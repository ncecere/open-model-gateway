pub mod audio;
pub mod client;
mod deadline_stream;
mod embeddings;
pub mod error;
pub(crate) mod evidence;
pub mod images;
pub mod realtime;
pub mod repository;
pub mod scheduling;
pub mod types;
pub mod workload;
pub mod workload_types;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
type SharedPermit = Arc<Mutex<Option<OwnedSemaphorePermit>>>;

use futures_util::StreamExt;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;

use crate::{auth::Principal, providers::ProviderRegistry, telemetry};
use error::InferenceError;
use repository::{AttemptTelemetry, ExecutionFinish, ExecutionStart, InferenceRepository, Outcome};
use types::{ApiProtocol, ChatEvent, ChatRequest, ProviderOutput, Usage};

/// `gen_ai.operation.name` of a protocol routed through the shared engine.
fn operation_of(protocol: ApiProtocol) -> telemetry::Operation {
    match protocol {
        ApiProtocol::ChatCompletions => telemetry::Operation::Chat,
        ApiProtocol::Responses => telemetry::Operation::Responses,
        ApiProtocol::Messages => telemetry::Operation::Messages,
        // Every other protocol is a non-generation workload, never routed
        // through `execute_protocol`.
        _ => telemetry::Operation::Chat,
    }
}
/// `http.route` of a protocol routed through the shared engine.
fn route_of(protocol: ApiProtocol) -> telemetry::Route {
    match protocol {
        ApiProtocol::ChatCompletions => telemetry::Route::Chat,
        ApiProtocol::Responses => telemetry::Route::Responses,
        ApiProtocol::Messages => telemetry::Route::Messages,
        _ => telemetry::Route::Chat,
    }
}
/// `gen_ai.provider.name`, or `None` for a provider kind outside the fixed
/// allowlist (never guessed, never a free-form fallback).
fn provider_of(provider: &str) -> Option<telemetry::Provider> {
    telemetry::Provider::ALL
        .iter()
        .copied()
        .find(|p| p.as_str() == provider)
}
/// `error.type` of a safe, already-public [`InferenceError`] variant. A
/// coarse, fixed mapping: several variants intentionally share one bucket.
fn error_kind_of(error: InferenceError) -> telemetry::ErrorKind {
    use telemetry::ErrorKind;
    match error {
        InferenceError::InvalidRequest => ErrorKind::InvalidRequest,
        InferenceError::Unauthenticated => ErrorKind::Unauthorized,
        InferenceError::ModelUnavailable => ErrorKind::NotFound,
        InferenceError::Unsupported => ErrorKind::Unsupported,
        InferenceError::Configuration | InferenceError::PriceUnbounded => {
            ErrorKind::Configuration
        }
        InferenceError::Busy | InferenceError::RouteCoolingDown(_) => ErrorKind::Busy,
        InferenceError::BudgetExceeded(_)
        | InferenceError::UnresolvedUsage(_)
        | InferenceError::TokenReservationExceedsLimit(_)
        | InferenceError::JobLimitExceeded(_) => ErrorKind::Forbidden,
        InferenceError::Timeout => ErrorKind::Timeout,
        InferenceError::UpstreamRejected
        | InferenceError::UpstreamUnavailable
        | InferenceError::InvalidUpstream => ErrorKind::Upstream,
        InferenceError::Storage => ErrorKind::Storage,
    }
}

/// Request-scoped trace context: created at engine entry (`inference.request`
/// is reserved before anything else, so routing/admission/attempt/settlement
/// spans can all name it as `parent`), carried across every failover attempt
/// and finished exactly once. Export never happens without a prior
/// [`telemetry::mark_shared`] commit deep in `governance.rs` for a shared
/// (team/project) workspace: a personal request, or one that never reaches
/// admission, just drops its buffered spans uncounted as exported
/// (`telemetry::Context`'s own `Drop` counts them `ineligible`).
struct RequestTelemetry {
    context: Option<telemetry::Context>,
    eligibility: Option<telemetry::SharedAdmission>,
    root: Option<[u8; 8]>,
    start: u64,
    operation: telemetry::Operation,
    streamed: bool,
    finished: bool,
}
impl RequestTelemetry {
    fn new(
        recorder: Option<&telemetry::Recorder>,
        request_id: Uuid,
        operation: telemetry::Operation,
        streamed: bool,
    ) -> Self {
        let context = recorder.and_then(|r| r.begin(request_id));
        let root = context.is_some().then(telemetry::reserve_span_id).flatten();
        Self {
            context,
            eligibility: None,
            root,
            start: telemetry::now_unix_nanos(),
            operation,
            streamed,
            finished: false,
        }
    }
    fn active(&self) -> bool {
        self.context.is_some()
    }
    fn trace(&self) -> Option<[u8; 16]> {
        self.context.as_ref().map(telemetry::Context::trace_id)
    }
    fn root(&self) -> Option<[u8; 8]> {
        self.root
    }
    fn record(&mut self, span: telemetry::Span) {
        if let Some(context) = self.context.as_mut() {
            context.record(span);
        }
    }
    /// Keeps the most recent eligibility a committed admission resolved for
    /// this request; never overwritten back to `None` by a later attempt
    /// that did not itself commit for a shared workspace.
    fn note_eligibility(&mut self, eligibility: Option<telemetry::SharedAdmission>) {
        if eligibility.is_some() {
            self.eligibility = eligibility;
        }
    }
    /// The admitted public alias, only once a commit has already proved
    /// shared eligibility for this exact request.
    fn alias(&self) -> Option<telemetry::PublicAlias> {
        self.context
            .as_ref()?
            .admitted_alias(self.eligibility.as_ref()?)
    }
    /// Finishes exactly once: the first call (explicit, at a terminal return
    /// in `execute_protocol`, or `Drop` for every other path) wins.
    fn finish(&mut self, outcome: telemetry::Outcome, error: Option<InferenceError>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(mut context) = self.context.take() else {
            return;
        };
        if let Some(root) = self.root {
            let end = telemetry::now_unix_nanos().max(self.start);
            if let Some(mut span) = telemetry::Span::reserved(
                telemetry::SpanName::InferenceRequest,
                context.trace_id(),
                root,
                None,
                self.start,
                end,
            ) {
                span.attribute(telemetry::Attribute::Operation(self.operation));
                span.attribute(telemetry::Attribute::Streamed(self.streamed));
                span.attribute(telemetry::Attribute::Outcome(outcome));
                if let Some(error) = error {
                    span.attribute(telemetry::Attribute::Error(error_kind_of(error)));
                }
                context.record(span);
            }
        }
        if let Some(eligibility) = self.eligibility.take() {
            context.export(eligibility);
        }
    }
}
impl Drop for RequestTelemetry {
    fn drop(&mut self) {
        // Every path that reaches here without an explicit `finish()` call
        // (a settlement write that failed or timed out, the request's own
        // timeout, or the stream simply being dropped, including before its
        // first poll) is one this crate does not treat as the request
        // durably completing its own work.
        self.finish(telemetry::Outcome::Cancelled, None);
    }
}

/// One ambient admission/settlement phase group: a reserved wrapper span id,
/// its start, and the [`telemetry::PhaseBuffer`] governance.rs's real phase
/// boundaries (`telemetry::mark_phase`) record into inside [`telemetry::with_phases`].
/// `None` is the allocation-free path when telemetry is inactive.
type PhaseGroup = Option<(Arc<Mutex<telemetry::PhaseBuffer>>, [u8; 8], u64)>;
fn begin_phase_group(telemetry: &RequestTelemetry) -> PhaseGroup {
    let trace = telemetry.trace()?;
    let id = telemetry::reserve_span_id()?;
    let start = telemetry::now_unix_nanos();
    Some((
        Arc::new(Mutex::new(telemetry::PhaseBuffer::new(trace, id, start))),
        id,
        start,
    ))
}
/// Merges one [`begin_phase_group`]'s collected sub-phase spans and any
/// [`telemetry::SharedAdmission`] eligibility into `telemetry`, then records
/// the wrapper span itself (`name`), with `attempt` as `gateway.attempt`
/// when it is `1..=3`.
fn end_phase_group(
    telemetry: &mut RequestTelemetry,
    group: PhaseGroup,
    name: telemetry::SpanName,
    attempt: usize,
) {
    let Some((buffer, id, start)) = group else {
        return;
    };
    if let Ok(mutex) = Arc::try_unwrap(buffer) {
        let (spans, eligibility) = mutex
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        for span in spans {
            telemetry.record(span);
        }
        telemetry.note_eligibility(eligibility);
    }
    let Some(trace) = telemetry.trace() else {
        return;
    };
    let Some(mut span) = telemetry::Span::reserved(
        name,
        trace,
        id,
        telemetry.root(),
        start,
        telemetry::now_unix_nanos(),
    ) else {
        return;
    };
    if let Ok(n) = u8::try_from(attempt) {
        span.attribute(telemetry::Attribute::Attempt(n));
    }
    telemetry.record(span);
}
/// Builds one `upstream.attempt` span: dispatch through its terminal
/// outcome. `None` when telemetry is inactive for this request.
#[allow(clippy::too_many_arguments)]
fn attempt_span(
    telemetry: &RequestTelemetry,
    start: u64,
    attempt_number: usize,
    provider: &str,
    streamed: bool,
    attempt_id: Uuid,
    outcome: telemetry::Outcome,
    error: Option<InferenceError>,
    usage: Usage,
    timings: &AttemptTelemetry,
) -> Option<telemetry::Span> {
    let trace = telemetry.trace()?;
    let mut span = telemetry::Span::new(
        telemetry::SpanName::UpstreamAttempt,
        trace,
        telemetry.root(),
        start,
        telemetry::now_unix_nanos(),
    )?;
    if let Ok(n) = u8::try_from(attempt_number) {
        span.attribute(telemetry::Attribute::Attempt(n));
    }
    if let Some(provider) = provider_of(provider) {
        span.attribute(telemetry::Attribute::Provider(provider));
    }
    span.attribute(telemetry::Attribute::Streamed(streamed));
    span.attribute(telemetry::Attribute::Outcome(outcome));
    if let Some(error) = error {
        span.attribute(telemetry::Attribute::Error(error_kind_of(error)));
    }
    if let Some(alias) = telemetry.alias() {
        span.attribute(telemetry::Attribute::Model(alias));
    }
    if let Some(n) = usage.input_tokens {
        span.attribute(telemetry::Attribute::InputTokens(n));
    }
    if let Some(n) = usage.output_tokens {
        span.attribute(telemetry::Attribute::OutputTokens(n));
    }
    if let Some(ms) = timings.time_to_first_token_ms {
        span.attribute(telemetry::Attribute::FirstContent(Duration::from_millis(ms)));
    }
    if let Some(ms) = timings.generation_ms {
        span.attribute(telemetry::Attribute::Generation(Duration::from_millis(ms)));
    }
    span.attribute(telemetry::Attribute::AttemptId(attempt_id));
    Some(span)
}

#[derive(Clone, Copy)]
pub struct EngineLimits {
    pub max_concurrent: usize,
    pub request_timeout: Duration,
    pub workloads: workload::WorkloadLimits,
    pub audio: audio::AudioLimits,
}
impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 128,
            request_timeout: Duration::from_secs(120),
            workloads: workload::WorkloadLimits::default(),
            audio: audio::AudioLimits::default(),
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    repository: Arc<dyn InferenceRepository>,
    registry: ProviderRegistry,
    capacity: Arc<Semaphore>,
    limits: EngineLimits,
    /// Realtime session limits (`Engine::with_realtime_limits`).
    realtime: realtime::RealtimeLimits,
    /// Bounded trace recorder (`Engine::with_telemetry`); None disables
    /// tracing, in which case the request path never allocates a
    /// RequestTelemetry context or hashes a request id.
    recorder: Option<telemetry::Recorder>,
}

impl Engine {
    pub fn new(
        repository: Arc<dyn InferenceRepository>,
        registry: ProviderRegistry,
        limits: EngineLimits,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            limits.max_concurrent > 0
                && limits.max_concurrent <= Semaphore::MAX_PERMITS
                && !limits.request_timeout.is_zero()
                && limits.workloads.validate()
                && limits.audio.validate(),
            "invalid inference limits"
        );
        Ok(Self {
            repository,
            registry,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent)),
            limits,
            realtime: realtime::RealtimeLimits::default(),
            recorder: None,
        })
    }

    /// Attaches the bounded trace recorder (#98). None disables tracing for
    /// every request through this engine.
    pub fn with_telemetry(mut self, recorder: Option<telemetry::Recorder>) -> Self {
        self.recorder = recorder;
        self
    }

    /// The same engine (adapters, limits, deadlines) with another repository
    /// and its own capacity, for background work such as gateway-run batch
    /// lines (`jobs::runner`), whose admission differs from interactive
    /// requests but whose execution path must not.
    pub fn with_repository(
        &self,
        repository: Arc<dyn InferenceRepository>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            repository,
            registry: self.registry.clone(),
            capacity: Arc::new(Semaphore::new(
                max_concurrent.clamp(1, Semaphore::MAX_PERMITS),
            )),
            limits: self.limits,
            realtime: self.realtime,
            // Background work (gateway-run batch lines) is out of scope for
            // interactive-generation tracing: never wrapped in telemetry,
            // same as every other non-interactive admission path.
            recorder: None,
        }
    }

    pub async fn execute(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<ProviderOutput, InferenceError> {
        self.execute_protocol(principal, request, request_id, ApiProtocol::ChatCompletions)
            .await
    }

    pub async fn execute_protocol(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
        protocol: ApiProtocol,
    ) -> Result<ProviderOutput, InferenceError> {
        let permit = Arc::new(Mutex::new(Some(
            self.capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| crate::metrics::capacity_denied())?,
        )));
        let started = Instant::now();
        let deadline = started + self.limits.request_timeout;
        let labels = client::current();
        // Reserved before anything else: routing/admission/attempt/
        // settlement spans all name this as `parent`. Export never happens
        // without a later shared-workspace admission commit (#98).
        let mut telemetry =
            RequestTelemetry::new(self.recorder.as_ref(), request_id, operation_of(protocol), request.stream);
        let routing_start = telemetry.active().then(telemetry::now_unix_nanos);
        let deployments = timeout_at(
            deadline,
            self.repository
                .candidate_deployments(&principal, &request.model, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        // Coarse protocol/capability gate (unchanged), then a pure,
        // deterministic per-candidate check (`ProviderAdapter::validate_chat_request`)
        // against the client's own request: a route that would refuse this
        // exact request deterministically is never durably admitted or
        // reserved for it. This is eligibility, not failover - a refusal here
        // consumes no attempt slot and produces no routing health
        // observation, and another authorized route may still accept the
        // same request.
        //
        // Validation depends only on the adapter and the client request, so it
        // runs once per adapter (provider id), not once per deployment: a model
        // with many routes on one provider must not multiply this unmetered,
        // pre-admission work.
        let mut candidates = Vec::new();
        let mut refused = Vec::new();
        let mut verdicts: std::collections::HashMap<String, Result<(), InferenceError>> =
            std::collections::HashMap::new();
        for deployment in deployments {
            if !deployment
                .supported_protocols
                .iter()
                .any(|p| p == protocol.as_str())
            {
                continue;
            }
            let Some(adapter) = self.registry.get(&deployment.provider) else {
                continue;
            };
            if !(adapter.supports_protocol(protocol) && adapter.supports_chat_request(&request)) {
                continue;
            }
            let verdict = *verdicts
                .entry(deployment.provider.clone())
                .or_insert_with(|| match adapter.validate_chat_request(&request) {
                    Ok(()) => Ok(()),
                    Err(InferenceError::InvalidRequest) => Err(InferenceError::InvalidRequest),
                    Err(InferenceError::Unsupported) => Err(InferenceError::Unsupported),
                    // The contract allows only InvalidRequest/Unsupported. A
                    // nonconforming adapter is treated as a capability refusal,
                    // never silently admitted, and reported for the operator.
                    Err(other) => {
                        tracing::warn!(provider = %deployment.provider, error = ?other,
                            "adapter request validation returned an error outside its contract");
                        Err(InferenceError::Unsupported)
                    }
                });
            match verdict {
                Ok(()) => candidates.push(deployment),
                Err(error) => refused.push((deployment, error)),
            }
        }
        if candidates.is_empty() {
            if refused.is_empty() {
                return Err(InferenceError::Unsupported);
            }
            // Every capable route refused the request. Report the refusal kind
            // only from routes the routing policy would actually use (residency,
            // operator-disabled routes and cooldown applied by the ordinary,
            // read-only planner), so an unusable route cannot change the
            // caller's error. Planning errors (nothing routable) surface as they
            // would for a valid request. Nothing is admitted or recorded here.
            let refused_deployments: Vec<_> = refused.iter().map(|(d, _)| d.clone()).collect();
            let usable = timeout_at(
                deadline,
                self.repository.route_plan(
                    &principal,
                    &request.model,
                    &refused_deployments,
                    request_id,
                ),
            )
            .await
            .map_err(|_| InferenceError::Timeout)??;
            let invalid = refused.iter().any(|(d, error)| {
                *error == InferenceError::InvalidRequest && usable.deployment_ids.contains(&d.id)
            });
            return Err(if invalid {
                InferenceError::InvalidRequest
            } else {
                InferenceError::Unsupported
            });
        }
        let plan = timeout_at(
            deadline,
            self.repository
                .route_plan(&principal, &request.model, &candidates, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if !(1..=3).contains(&plan.max_attempts) {
            return Err(InferenceError::Configuration);
        }
        if let Some(start) = routing_start
            && let Some(trace) = telemetry.trace()
            && let Some(mut span) = telemetry::Span::new(
                telemetry::SpanName::Routing,
                trace,
                telemetry.root(),
                start,
                telemetry::now_unix_nanos(),
            )
        {
            span.attribute(telemetry::Attribute::Route(route_of(protocol)));
            telemetry.record(span);
        }
        let streamed = request.stream;
        for (attempt, id) in plan
            .deployment_ids
            .iter()
            .take(plan.max_attempts)
            .enumerate()
        {
            let deployment = candidates
                .iter()
                .find(|d| d.id == *id)
                .ok_or(InferenceError::Configuration)?;
            let adapter = self
                .registry
                .get(&deployment.provider)
                .ok_or(InferenceError::Configuration)?;
            let execution_id = if attempt == 0 {
                request_id
            } else {
                Uuid::now_v7()
            };
            let attempt_started = Instant::now();
            let attempt_wall_start = telemetry.active().then(telemetry::now_unix_nanos);
            let admission_group = begin_phase_group(&telemetry);
            let execution_start = ExecutionStart {
                id: execution_id,
                root_request_id: request_id,
                attempt_number: (attempt + 1) as i32,
                principal,
                deployment_id: deployment.id,
                provider: deployment.provider.clone(),
                model: request.model.clone(),
                streamed,
                upstream_model: upstream_snapshot(&deployment.upstream_model),
                client: labels.clone(),
            };
            let admission_future = timeout_at(
                deadline,
                self.repository.admit(
                    &execution_start,
                    &request,
                    self.limits
                        .request_timeout
                        .as_secs()
                        .min(i64::MAX as u64 - 5) as i64
                        + 5,
                    deployment,
                    adapter.generation_output_limit(),
                ),
            );
            let admission_result = match admission_group.as_ref().map(|(b, _, _)| b.clone()) {
                Some(buffer) => telemetry::with_phases(buffer, admission_future).await,
                None => admission_future.await,
            };
            end_phase_group(
                &mut telemetry,
                admission_group,
                telemetry::SpanName::Admission,
                attempt + 1,
            );
            let admission = admission_result.map_err(|_| InferenceError::Timeout)??;
            let mut guard = ExecutionGuard::new(
                self.repository.clone(),
                execution_id,
                deployment.id,
                attempt_started,
                permit.clone(),
            );
            // This clone belongs only to this attempt. A failover resolves its
            // own default from its own immutable price, never from this cap.
            let mut attempt_request = request.clone();
            attempt_request.max_output_tokens = admission.max_output_tokens;
            let (output, invalid_body_usage) = evidence::capture(timeout_at(
                deadline,
                adapter.execute_protocol(deployment, attempt_request, protocol),
            ))
            .await;
            // A body that failed validation still fails, but its valid usage
            // object is retained as observed evidence (the hold is kept).
            if let Some(observed) = invalid_body_usage.filter(|u| valid_usage(*u)) {
                guard.usage = observed;
            }
            let output = match output {
                Ok(result) => result,
                Err(_) => {
                    guard.health_observation = false;
                    Err(InferenceError::Timeout)
                }
            };
            match output {
                Err(error) => {
                    let settled = settle_attempt(
                        &mut telemetry,
                        &mut guard,
                        Outcome::Failed,
                        Some(error),
                        attempt + 1,
                        attempt_wall_start,
                        execution_id,
                        &deployment.provider,
                        streamed,
                    )
                    .await;
                    let failing_over = settled.is_ok()
                        && attempt + 1 < plan.max_attempts
                        && attempt + 1 < plan.deployment_ids.len()
                        && crate::routing::may_failover(&plan, error);
                    if failing_over {
                        continue;
                    }
                    if let Err(settlement_error) = settled {
                        telemetry.finish(telemetry::Outcome::Error, Some(settlement_error));
                        return Err(settlement_error);
                    }
                    telemetry.finish(telemetry::Outcome::Error, Some(error));
                    return Err(error);
                }
                Ok(ProviderOutput::Complete(response)) if !streamed => {
                    if !valid_usage(response.usage) {
                        let settled = settle_attempt(
                            &mut telemetry,
                            &mut guard,
                            Outcome::Failed,
                            Some(InferenceError::InvalidUpstream),
                            attempt + 1,
                            attempt_wall_start,
                            execution_id,
                            &deployment.provider,
                            streamed,
                        )
                        .await;
                        let returned = settled.err().unwrap_or(InferenceError::InvalidUpstream);
                        telemetry.finish(telemetry::Outcome::Error, Some(returned));
                        return Err(returned);
                    }
                    guard.usage = response.usage;
                    guard.telemetry.finish_reason = Some(response.finish_reason.into());
                    let settled = settle_attempt(
                        &mut telemetry,
                        &mut guard,
                        Outcome::Succeeded,
                        None,
                        attempt + 1,
                        attempt_wall_start,
                        execution_id,
                        &deployment.provider,
                        streamed,
                    )
                    .await;
                    match settled {
                        Ok(()) => {
                            telemetry.finish(telemetry::Outcome::Completed, None);
                            return Ok(ProviderOutput::Complete(response));
                        }
                        Err(error) => {
                            telemetry.finish(telemetry::Outcome::Error, Some(error));
                            return Err(error);
                        }
                    }
                }
                Ok(ProviderOutput::Stream(upstream)) if streamed => {
                    // The watchdog drops transport at the deadline even if a slow
                    // downstream never polls the returned stream again.
                    let mut upstream = deadline_stream::DeadlineStream::new(
                        upstream,
                        deadline,
                        Some(permit.clone()),
                    );
                    let streaming_provider = deployment.provider.clone();
                    let tracked = async_stream::stream! {
                        // Capturing guard and telemetry outside the generator
                        // accounts for drops even before the first poll: both
                        // finish/export or drop-count exactly once via their
                        // own Drop.
                        let mut guard = guard;
                        let mut telemetry = telemetry;
                        let mut finished_choice = false;
                        let mut reported_usage = false;
                        loop {
                            let (polled, invalid_body_usage) = evidence::capture(timeout_at(deadline, upstream.next())).await;
                            if let Some(observed) = invalid_body_usage.filter(|u| !reported_usage && valid_usage(*u)) {
                                guard.usage = observed;
                            }
                            let next = match polled {
                                Ok(Some(item)) => item,
                                Ok(None) => Err(InferenceError::InvalidUpstream),
                                Err(_) => Err(InferenceError::Timeout),
                            };
                            if matches!(next,Err(InferenceError::Timeout)) && Instant::now()>=deadline {guard.health_observation=false;}
                            let item = match next {
                                Ok(ChatEvent::Delta { .. }) if finished_choice => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Delta { text, tool_calls }) => { guard.first_token(text.as_deref(), &tool_calls); Ok(ChatEvent::Delta { text, tool_calls }) }
                                Ok(ChatEvent::Finish(_)) if finished_choice => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Finish(reason)) => { finished_choice = true; guard.telemetry.finish_reason = Some(reason.into()); Ok(ChatEvent::Finish(reason)) }
                                Ok(ChatEvent::Usage(usage)) if reported_usage || !valid_usage(usage) => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Usage(usage)) => { reported_usage = true; guard.usage = usage; Ok(ChatEvent::Usage(usage)) }
                                Ok(ChatEvent::Done) if !finished_choice => Err(InferenceError::InvalidUpstream),
                                other => other,
                            };
                            match item {
                                Ok(ChatEvent::Done) => {
                                    let settled = settle_attempt(&mut telemetry, &mut guard, Outcome::Succeeded, None, attempt + 1, attempt_wall_start, execution_id, &streaming_provider, streamed).await;
                                    match settled {
                                        Ok(()) => {
                                            telemetry.finish(telemetry::Outcome::Completed, None);
                                            yield Ok(ChatEvent::Done);
                                        }
                                        Err(error) => {
                                            telemetry.finish(telemetry::Outcome::Error, Some(error));
                                            yield Err(error);
                                        }
                                    }
                                    break;
                                }
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    let settled = settle_attempt(&mut telemetry, &mut guard, Outcome::Failed, Some(error), attempt + 1, attempt_wall_start, execution_id, &streaming_provider, streamed).await;
                                    let error = settled.err().unwrap_or(error);
                                    telemetry.finish(telemetry::Outcome::Error, Some(error));
                                    yield Err(error);
                                    break;
                                }
                            }
                        }
                    };
                    return Ok(ProviderOutput::Stream(Box::pin(tracked)));
                }
                Ok(_) => {
                    let settled = settle_attempt(
                        &mut telemetry,
                        &mut guard,
                        Outcome::Failed,
                        Some(InferenceError::InvalidUpstream),
                        attempt + 1,
                        attempt_wall_start,
                        execution_id,
                        &deployment.provider,
                        streamed,
                    )
                    .await;
                    let returned = settled.err().unwrap_or(InferenceError::InvalidUpstream);
                    telemetry.finish(telemetry::Outcome::Error, Some(returned));
                    return Err(returned);
                }
            }
        }
        Err(InferenceError::Busy)
    }
}

/// The configured upstream model id as stored (1..=512 characters), else none.
fn upstream_snapshot(model: &str) -> Option<String> {
    (1..=512)
        .contains(&model.chars().count())
        .then(|| model.to_owned())
}

fn valid_usage(usage: Usage) -> bool {
    usage.meters.is_none_or(|m| m.validate().is_ok())
        && usage.provider_cost_microusd.is_none_or(|n| n >= 0)
        && valid_token_usage(usage)
}
fn valid_token_usage(usage: Usage) -> bool {
    [usage.input_tokens, usage.output_tokens]
        .into_iter()
        .flatten()
        .all(|n| n <= i64::MAX as u64)
        && u128::from(usage.input_tokens.unwrap_or(0))
            + u128::from(usage.output_tokens.unwrap_or(0))
            <= i64::MAX as u128
        && usage.billing.is_none_or(|billing| {
            billing.input_lower_bound().is_ok_and(|n| {
                u128::from(n) + u128::from(usage.output_tokens.unwrap_or(0)) <= i64::MAX as u128
            }) && billing.validate().is_ok()
                && match (billing.total_input_tokens, usage.input_tokens) {
                    (Some(total), Some(raw)) => total >= raw,
                    _ => true,
                }
        })
}

/// Settles one attempt's durable write (wrapping its real phase boundaries
/// as a `settlement` span, via [`begin_phase_group`]/[`end_phase_group`])
/// and records its `upstream.attempt` span, regardless of whether the
/// settlement write itself succeeded. Returns settlement's own result
/// unchanged, so callers keep deciding failover/return exactly as before.
#[allow(clippy::too_many_arguments)]
async fn settle_attempt(
    telemetry: &mut RequestTelemetry,
    guard: &mut ExecutionGuard,
    outcome: Outcome,
    error: Option<InferenceError>,
    attempt_number: usize,
    attempt_wall_start: Option<u64>,
    attempt_id: Uuid,
    provider: &str,
    streamed: bool,
) -> Result<(), InferenceError> {
    let telemetry_outcome = match outcome {
        Outcome::Succeeded => telemetry::Outcome::Completed,
        Outcome::Failed => telemetry::Outcome::Error,
        Outcome::Cancelled => telemetry::Outcome::Cancelled,
    };
    let usage = guard.usage;
    let timings = guard.telemetry_now();
    let group = begin_phase_group(telemetry);
    let buffer = group.as_ref().map(|(b, _, _)| b.clone());
    let result = guard.finish(outcome, error, buffer).await;
    end_phase_group(
        telemetry,
        group,
        telemetry::SpanName::Settlement,
        attempt_number,
    );
    if let Some(start) = attempt_wall_start
        && let Some(span) = attempt_span(
            telemetry,
            start,
            attempt_number,
            provider,
            streamed,
            attempt_id,
            telemetry_outcome,
            error,
            usage,
            &timings,
        )
    {
        telemetry.record(span);
    }
    result
}

struct ExecutionGuard {
    repository: Arc<dyn InferenceRepository>,
    id: Uuid,
    deployment_id: Uuid,
    started: Instant,
    /// Upstream dispatch (after admission): the origin of attempt timings.
    dispatched: Instant,
    usage: Usage,
    telemetry: AttemptTelemetry,
    finished: bool,
    health_observation: bool,
    _permit: SharedPermit,
}
fn millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}
impl ExecutionGuard {
    /// Created right after durable admission, immediately before dispatch.
    fn new(
        repository: Arc<dyn InferenceRepository>,
        id: Uuid,
        deployment_id: Uuid,
        started: Instant,
        permit: SharedPermit,
    ) -> Self {
        Self {
            repository,
            id,
            deployment_id,
            started,
            dispatched: Instant::now(),
            usage: Usage::default(),
            telemetry: AttemptTelemetry::default(),
            finished: false,
            health_observation: true,
            _permit: permit,
        }
    }
    /// Streams: the first delta carrying text or a tool call sets time to first token.
    fn first_token(&mut self, text: Option<&str>, tool_calls: &[types::ToolCallDelta]) {
        if self.telemetry.time_to_first_token_ms.is_none()
            && (text.is_some_and(|t| !t.is_empty()) || !tool_calls.is_empty())
        {
            self.telemetry.time_to_first_token_ms = Some(millis(self.dispatched.elapsed()));
        }
    }
    /// Telemetry as of now: generation time ends when the attempt finishes.
    fn telemetry_now(&self) -> AttemptTelemetry {
        AttemptTelemetry {
            generation_ms: self
                .telemetry
                .generation_ms
                .or(Some(millis(self.dispatched.elapsed()))),
            ..self.telemetry
        }
    }
    fn record(&self, outcome: Outcome, error: Option<InferenceError>) -> ExecutionFinish {
        ExecutionFinish {
            id: self.id,
            outcome,
            error,
            usage: self.usage,
            elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        }
    }
    async fn finish(
        &mut self,
        outcome: Outcome,
        error: Option<InferenceError>,
        phases: Option<Arc<Mutex<telemetry::PhaseBuffer>>>,
    ) -> Result<(), InferenceError> {
        // Mark attempted finalization first: failed writes remain 'started' for reconciliation.
        self.finished = true;
        let record = self.record(outcome, error);
        let attempt_telemetry = self.telemetry_now();
        let repository = self.repository.clone();
        let result = timeout(Duration::from_secs(3), async move {
            let settle = repository.finish_attempt(&record, &attempt_telemetry);
            match phases {
                Some(buffer) => telemetry::with_phases(buffer, settle).await,
                None => settle.await,
            }
        })
        .await
        .map_err(|_| {
            // A write that fails fast counts itself as held inside
            // `finish_attempt`; one that times out never gets that far (#136).
            crate::metrics::METRICS.observe_settlement(crate::metrics::Settlement::Held);
            InferenceError::Storage
        })
        .and_then(|result| result);
        if result.is_err() {
            tracing::error!(execution_id = %self.id, "inference finalization failed; reconciliation required");
        }
        // Health is advisory; a failed health write never discards a durably accounted response.
        if self.health_observation
            && outcome != Outcome::Cancelled
            && !matches!(
                timeout(
                    Duration::from_millis(250),
                    self.repository.route_result(self.deployment_id, error)
                )
                .await,
                Ok(Ok(()))
            )
        {
            tracing::warn!(execution_id=%self.id,"routing health update failed");
        }
        result
    }
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let repository = self.repository.clone();
        let record = self.record(Outcome::Cancelled, None);
        let telemetry = self.telemetry_now();
        // Network work is dropped synchronously. Only best-effort accounting is detached.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let finished = timeout(Duration::from_secs(3), repository.finish_attempt(&record, &telemetry)).await;
                if finished.is_err() {
                    crate::metrics::METRICS.observe_settlement(crate::metrics::Settlement::Held);
                }
                if !matches!(finished, Ok(Ok(()))) {
                    tracing::error!(execution_id = %record.id, "cancelled inference finalization failed; reconciliation required");
                }
            });
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod admission_validation_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod steering_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod telemetry_tests;
