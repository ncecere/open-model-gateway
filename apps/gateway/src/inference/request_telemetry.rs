//! Request-scoped trace context and the span helpers the engine records with:
//! the `inference.request` root, the per-attempt spans and the ambient
//! admission/settlement phase groups. Spans never carry request content.
use super::*;

/// `gen_ai.operation.name` of a protocol routed through the shared engine.
pub(super) fn operation_of(protocol: ApiProtocol) -> telemetry::Operation {
    match protocol {
        ApiProtocol::ChatCompletions => telemetry::Operation::Chat,
        ApiProtocol::Responses => telemetry::Operation::Responses,
        ApiProtocol::Messages => telemetry::Operation::Messages,
        // Every other protocol is a non-generation workload, never routed
        // through `execute_protocol`. Caught here only as a safety net:
        // debug builds and tests fail loudly instead of silently mislabeling
        // the operation if that assumption is ever violated.
        other => {
            debug_assert!(
                false,
                "operation_of called for non-generation protocol {other:?}"
            );
            telemetry::Operation::Chat
        }
    }
}
/// `http.route` of a protocol routed through the shared engine.
pub(super) fn route_of(protocol: ApiProtocol) -> telemetry::Route {
    match protocol {
        ApiProtocol::ChatCompletions => telemetry::Route::Chat,
        ApiProtocol::Responses => telemetry::Route::Responses,
        ApiProtocol::Messages => telemetry::Route::Messages,
        other => {
            debug_assert!(
                false,
                "route_of called for non-generation protocol {other:?}"
            );
            telemetry::Route::Chat
        }
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
        InferenceError::InvalidRequest
        | InferenceError::InvalidField(_)
        | InferenceError::UnknownField(_)
        | InferenceError::ContextLength => ErrorKind::InvalidRequest,
        InferenceError::Unauthenticated => ErrorKind::Unauthorized,
        InferenceError::ModelUnavailable => ErrorKind::NotFound,
        InferenceError::Unsupported => ErrorKind::Unsupported,
        InferenceError::Configuration | InferenceError::PriceUnbounded => ErrorKind::Configuration,
        InferenceError::Busy | InferenceError::Overloaded | InferenceError::RouteCoolingDown(_) => {
            ErrorKind::Busy
        }
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
pub(super) struct RequestTelemetry {
    context: Option<telemetry::Context>,
    eligibility: Option<telemetry::SharedAdmission>,
    root: Option<[u8; 8]>,
    start: u64,
    operation: telemetry::Operation,
    streamed: bool,
    finished: bool,
}
impl RequestTelemetry {
    pub(super) fn new(
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
    pub(super) fn active(&self) -> bool {
        self.context.is_some()
    }
    pub(super) fn trace(&self) -> Option<[u8; 16]> {
        self.context.as_ref().map(telemetry::Context::trace_id)
    }
    pub(super) fn root(&self) -> Option<[u8; 8]> {
        self.root
    }
    pub(super) fn record(&mut self, span: telemetry::Span) {
        if let Some(context) = self.context.as_mut() {
            context.record(span);
        }
    }
    /// Counts one span that could never be built at all (#98 PR 2 review
    /// item 5), a no-op while telemetry is inactive for this request.
    pub(super) fn drop_invalid(&self) {
        if let Some(context) = self.context.as_ref() {
            context.drop_invalid();
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
    /// This request's trace id, only once a commit has already proved
    /// shared eligibility for it (log-line correlation, #98): `None` for a
    /// personal workspace or while telemetry is inactive.
    pub(super) fn trace_from_eligibility(&self) -> Option<[u8; 16]> {
        self.eligibility.as_ref()?;
        self.context.as_ref().map(telemetry::Context::trace_id)
    }
    /// Finishes exactly once: the first call (explicit, at a terminal return
    /// of the request or an attempt, or `Drop` for every other path) wins.
    pub(super) fn finish(&mut self, outcome: telemetry::Outcome, error: Option<InferenceError>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(mut context) = self.context.take() else {
            return;
        };
        if let Some(root) = self.root {
            let end = telemetry::now_unix_nanos().max(self.start);
            match telemetry::Span::reserved(
                telemetry::SpanName::InferenceRequest,
                context.trace_id(),
                root,
                None,
                self.start,
                end,
            ) {
                Some(mut span) => {
                    span.attribute(telemetry::Attribute::Operation(self.operation));
                    span.attribute(telemetry::Attribute::Streamed(self.streamed));
                    span.attribute(telemetry::Attribute::Outcome(outcome));
                    if let Some(error) = error {
                        span.attribute(telemetry::Attribute::Error(error_kind_of(error)));
                    }
                    // Reserved outside the ordinary per-request child-span
                    // budget (#98 PR 2 review item 6): this is the
                    // request's sole root span, recorded exactly once here
                    // (guarded by `self.finished` above), so it must never
                    // be the span a full budget of children drops.
                    context.record_root(span);
                }
                // #98 PR 2 review item 5: the root span could not be built
                // at all (a malformed shape, or entropy exhaustion) -
                // counted, never a silent, uncounted loss.
                None => context.drop_invalid(),
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
/// its start, and the [`telemetry::PhaseBuffer`] the governance module's real phase
/// boundaries (`telemetry::mark_phase`) record into inside [`telemetry::with_phases`].
/// `None` is the allocation-free path when telemetry is inactive.
type PhaseGroup = Option<(Arc<Mutex<telemetry::PhaseBuffer>>, [u8; 8], u64)>;
pub(super) fn begin_phase_group(telemetry: &RequestTelemetry) -> PhaseGroup {
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
pub(super) fn end_phase_group(
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
    let mut span = match telemetry::Span::reserved(
        name,
        trace,
        id,
        telemetry.root(),
        start,
        telemetry::now_unix_nanos(),
    ) {
        Some(span) => span,
        // #98 PR 2 review item 5: counted, never a silent, uncounted loss.
        None => {
            telemetry.drop_invalid();
            return;
        }
    };
    if let Ok(n) = u8::try_from(attempt) {
        span.attribute(telemetry::Attribute::Attempt(n));
    }
    telemetry.record(span);
}
/// Builds one `upstream.attempt` span: dispatch through its terminal
/// outcome. `None` when telemetry is inactive for this request.
#[allow(clippy::too_many_arguments)]
pub(super) fn attempt_span(
    telemetry: &RequestTelemetry,
    start: u64,
    end: u64,
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
    let mut span = match telemetry::Span::new(
        telemetry::SpanName::UpstreamAttempt,
        trace,
        telemetry.root(),
        start,
        end,
    ) {
        Some(span) => span,
        // #98 PR 2 review item 5: counted, never a silent, uncounted loss.
        None => {
            telemetry.drop_invalid();
            return None;
        }
    };
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
        span.attribute(telemetry::Attribute::FirstContent(Duration::from_millis(
            ms,
        )));
    }
    if let Some(ms) = timings.generation_ms {
        span.attribute(telemetry::Attribute::Generation(Duration::from_millis(ms)));
    }
    span.attribute(telemetry::Attribute::AttemptId(attempt_id));
    Some(span)
}
