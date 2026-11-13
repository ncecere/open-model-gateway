//! One request's route eligibility and its attempts: admission, the upstream
//! call, settlement of each outcome and the streamed-response tracker.
use super::*;

impl Engine {
    /// Admits one attempt inside its `admission` phase group, then records
    /// the group. A timeout is `InferenceError::Timeout`.
    async fn admit(
        &self,
        scope: &RequestScope<'_>,
        telemetry: &mut RequestTelemetry,
        attempt: usize,
        execution_id: Uuid,
        deployment: &Deployment,
        adapter: &dyn crate::providers::ProviderAdapter,
    ) -> Result<repository::GenerationAdmission, InferenceError> {
        let RequestScope {
            principal,
            request,
            request_id,
            deadline,
            labels,
            streamed,
            ..
        } = *scope;
        let admission_group = begin_phase_group(telemetry);
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
                request,
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
            telemetry,
            admission_group,
            telemetry::SpanName::Admission,
            attempt + 1,
        );
        admission_result
            .map_err(|_| InferenceError::Timeout)
            .and_then(|admitted| admitted)
    }

    /// One attempt on `deployment`: admission, the upstream call and the
    /// settlement of its outcome. Gives the request's telemetry back when the
    /// upstream call failed, its settlement succeeded and the plan allows a
    /// failover; otherwise the request's result.
    pub(super) async fn attempt(
        &self,
        scope: &RequestScope<'_>,
        mut telemetry: RequestTelemetry,
        attempt: usize,
        deployment: &Deployment,
        adapter: Arc<dyn crate::providers::ProviderAdapter>,
    ) -> AttemptEnd {
        let RequestScope {
            request,
            request_id,
            deadline,
            permit,
            plan,
            protocol,
            streamed,
            ..
        } = *scope;
        let execution_id = if attempt == 0 {
            request_id
        } else {
            Uuid::now_v7()
        };
        let attempt_started = Instant::now();
        let attempt_wall_start = telemetry.active().then(telemetry::now_unix_nanos);
        let admission = match self
            .admit(
                scope,
                &mut telemetry,
                attempt,
                execution_id,
                deployment,
                &*adapter,
            )
            .await
        {
            Ok(admission) => admission,
            Err(error) => {
                // #98 PR 2 review item 2: the timeout and the admission's
                // own error (e.g. a team-workspace rate or budget
                // denial) both return here, before any
                // `ExecutionGuard`/settlement path exists for this
                // attempt - so, like every other terminal return in this
                // function, telemetry must be finished explicitly here
                // rather than left for Drop to mislabel `cancelled`.
                telemetry.finish(telemetry::Outcome::Error, Some(error));
                return AttemptEnd::Return(Err(error));
            }
        };
        // Logging-only correlation (#98): known only once a
        // shared-workspace admission has already committed for this
        // request; never for personal workspaces.
        let log_trace_id = telemetry
            .trace_from_eligibility()
            .map(hex::encode)
            .map(LogTraceId::from_provenance);
        let mut guard = ExecutionGuard::new(
            self.repository.clone(),
            execution_id,
            deployment.id,
            attempt_started,
            permit.clone(),
            log_trace_id,
        );
        let output = call_upstream(
            &mut guard, &*adapter, deployment, request, &admission, deadline, protocol,
        )
        .await;
        let span = AttemptSpan {
            number: attempt + 1,
            wall_start: attempt_wall_start,
            id: execution_id,
            provider: &deployment.provider,
            streamed,
        };
        match output {
            Err(error) => {
                let settled = span
                    .settle(&mut telemetry, &mut guard, Outcome::Failed, Some(error))
                    .await;
                let failing_over = settled.is_ok()
                    && attempt + 1 < plan.max_attempts
                    && attempt + 1 < plan.deployment_ids.len()
                    && crate::routing::may_failover(plan, error);
                if failing_over {
                    return AttemptEnd::Failover(telemetry);
                }
                if let Err(settlement_error) = settled {
                    telemetry.finish(telemetry::Outcome::Error, Some(settlement_error));
                    return AttemptEnd::Return(Err(settlement_error));
                }
                telemetry.finish(telemetry::Outcome::Error, Some(error));
                AttemptEnd::Return(Err(error))
            }
            Ok(ProviderOutput::Complete(response)) if !streamed => AttemptEnd::Return(
                settle_complete(&mut telemetry, &mut guard, span, response).await,
            ),
            Ok(ProviderOutput::Stream(upstream)) if streamed => {
                AttemptEnd::Return(Ok(ProviderOutput::Stream(track_stream(
                    upstream,
                    guard,
                    telemetry,
                    deadline,
                    permit.clone(),
                    span,
                ))))
            }
            Ok(_) => AttemptEnd::Return(Err(settle_invalid_upstream(
                &mut telemetry,
                &mut guard,
                span,
            )
            .await)),
        }
    }

    /// Splits `deployments` into eligible candidates and refused routes.
    ///
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
    pub(super) fn eligible_candidates(
        &self,
        deployments: Vec<Deployment>,
        request: &ChatRequest,
        protocol: ApiProtocol,
    ) -> (Vec<Deployment>, Vec<(Deployment, InferenceError)>) {
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
            if !(adapter.supports_protocol(protocol) && adapter.supports_chat_request(request)) {
                continue;
            }
            let verdict = *verdicts
                .entry(deployment.provider.clone())
                .or_insert_with(|| match adapter.validate_chat_request(request) {
                    Ok(()) => Ok(()),
                    Err(e @ (InferenceError::InvalidRequest | InferenceError::InvalidField(_))) => {
                        Err(e)
                    }
                    Err(InferenceError::Unsupported) => Err(InferenceError::Unsupported),
                    // The contract allows only InvalidRequest/InvalidField/Unsupported. A
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
        (candidates, refused)
    }

    /// The error for a request every capable route refused. Report the refusal kind
    /// only from routes the routing policy would actually use (residency,
    /// operator-disabled routes and cooldown applied by the ordinary,
    /// read-only planner), so an unusable route cannot change the
    /// caller's error. Planning errors (nothing routable) surface as they
    /// would for a valid request. Nothing is admitted or recorded here.
    pub(super) async fn refuse(
        &self,
        principal: &Principal,
        request: &ChatRequest,
        refused: &[(Deployment, InferenceError)],
        request_id: Uuid,
        deadline: Instant,
    ) -> Result<ProviderOutput, InferenceError> {
        let refused_deployments: Vec<_> = refused.iter().map(|(d, _)| d.clone()).collect();
        let usable = timeout_at(
            deadline,
            self.repository
                .route_plan(principal, &request.model, &refused_deployments, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        let invalid = refused.iter().find(|(d, error)| {
            matches!(
                error,
                InferenceError::InvalidRequest | InferenceError::InvalidField(_)
            ) && usable.deployment_ids.contains(&d.id)
        });
        Err(invalid.map_or(InferenceError::Unsupported, |(_, error)| *error))
    }
}

/// What every attempt of one request shares.
#[derive(Clone, Copy)]
pub(super) struct RequestScope<'a> {
    pub(super) principal: Principal,
    pub(super) request: &'a ChatRequest,
    pub(super) request_id: Uuid,
    pub(super) deadline: Instant,
    pub(super) labels: &'a client::ClientMetadata,
    pub(super) permit: &'a SharedPermit,
    pub(super) plan: &'a crate::routing::RoutePlan,
    pub(super) protocol: ApiProtocol,
    pub(super) streamed: bool,
}

/// How one attempt ended: failed with a failover allowed (the request's
/// telemetry comes back for the next attempt), or with the request's result.
#[expect(
    clippy::large_enum_variant,
    reason = "returned by value once per attempt, like the Result it wraps; boxing would allocate per request"
)]
pub(super) enum AttemptEnd {
    Failover(RequestTelemetry),
    Return(Result<ProviderOutput, InferenceError>),
}

/// Records the `routing` span from `start` to now when telemetry is active.
pub(super) fn record_routing_span(
    telemetry: &mut RequestTelemetry,
    routing_start: Option<u64>,
    protocol: ApiProtocol,
) {
    if let Some(start) = routing_start
        && let Some(trace) = telemetry.trace()
    {
        match telemetry::Span::new(
            telemetry::SpanName::Routing,
            trace,
            telemetry.root(),
            start,
            telemetry::now_unix_nanos(),
        ) {
            Some(mut span) => {
                span.attribute(telemetry::Attribute::Route(route_of(protocol)));
                telemetry.record(span);
            }
            // #98 PR 2 review item 5: counted, never a silent,
            // uncounted loss.
            None => telemetry.drop_invalid(),
        }
    }
}

/// Calls the upstream for one admitted attempt with the admission's output
/// cap, recording any usage evidence on `guard`. A deadline timeout is
/// `InferenceError::Timeout` and is not a health observation.
async fn call_upstream(
    guard: &mut ExecutionGuard,
    adapter: &dyn crate::providers::ProviderAdapter,
    deployment: &Deployment,
    request: &ChatRequest,
    admission: &repository::GenerationAdmission,
    deadline: Instant,
    protocol: ApiProtocol,
) -> Result<ProviderOutput, InferenceError> {
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
    match output {
        Ok(result) => result,
        Err(_) => {
            guard.health_observation = false;
            Err(InferenceError::Timeout)
        }
    }
}

/// What [`settle_attempt`] records about one attempt besides its outcome.
#[derive(Clone, Copy)]
struct AttemptSpan<'a> {
    number: usize,
    wall_start: Option<u64>,
    id: Uuid,
    provider: &'a str,
    streamed: bool,
}

impl AttemptSpan<'_> {
    async fn settle(
        self,
        telemetry: &mut RequestTelemetry,
        guard: &mut ExecutionGuard,
        outcome: Outcome,
        error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        settle_attempt(
            telemetry,
            guard,
            outcome,
            error,
            self.number,
            self.wall_start,
            self.id,
            self.provider,
            self.streamed,
        )
        .await
    }
}

/// Settles a non-streamed attempt that returned a complete response:
/// invalid usage fails the attempt; otherwise it succeeds with the
/// response's usage and finish reason.
async fn settle_complete(
    telemetry: &mut RequestTelemetry,
    guard: &mut ExecutionGuard,
    span: AttemptSpan<'_>,
    response: types::ChatResponse,
) -> Result<ProviderOutput, InferenceError> {
    if !valid_usage(response.usage) {
        return Err(settle_invalid_upstream(telemetry, guard, span).await);
    }
    guard.usage = response.usage;
    guard.telemetry.finish_reason = Some(response.finish_reason.into());
    let settled = span
        .settle(telemetry, guard, Outcome::Succeeded, None)
        .await;
    match settled {
        Ok(()) => {
            telemetry.finish(telemetry::Outcome::Completed, None);
            Ok(ProviderOutput::Complete(response))
        }
        Err(error) => {
            telemetry.finish(telemetry::Outcome::Error, Some(error));
            Err(error)
        }
    }
}

/// Fails an attempt whose upstream output was invalid, and returns the error
/// the caller sees: settlement's own error if settlement failed, else
/// `InvalidUpstream`.
async fn settle_invalid_upstream(
    telemetry: &mut RequestTelemetry,
    guard: &mut ExecutionGuard,
    span: AttemptSpan<'_>,
) -> InferenceError {
    let settled = span
        .settle(
            telemetry,
            guard,
            Outcome::Failed,
            Some(InferenceError::InvalidUpstream),
        )
        .await;
    let returned = settled.err().unwrap_or(InferenceError::InvalidUpstream);
    telemetry.finish(telemetry::Outcome::Error, Some(returned));
    returned
}

/// Wraps a streamed attempt's upstream events: validates their order and
/// usage, records the first token, and settles the attempt exactly once at
/// `Done` or the first error.
fn track_stream(
    upstream: types::EventStream,
    guard: ExecutionGuard,
    telemetry: RequestTelemetry,
    deadline: Instant,
    permit: SharedPermit,
    span: AttemptSpan<'_>,
) -> types::EventStream {
    // The watchdog drops transport at the deadline even if a slow
    // downstream never polls the returned stream again.
    let mut upstream = deadline_stream::DeadlineStream::new(upstream, deadline, Some(permit));
    // The generator outlives `span`'s borrow, so it owns the provider id.
    let streaming_provider = span.provider.to_owned();
    let (number, wall_start, id, streamed) = (span.number, span.wall_start, span.id, span.streamed);
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
                Ok(ChatEvent::MessagesBlock(_)) if finished_choice => Err(InferenceError::InvalidUpstream),
                Ok(ChatEvent::MessagesBlock(value)) => {
                    let text = value["delta"]["thinking"].as_str().or_else(|| value["content_block"]["thinking"].as_str());
                    guard.first_token(text, &[]);
                    Ok(ChatEvent::MessagesBlock(value))
                }
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
                    let settled = AttemptSpan { number, wall_start, id, provider: &streaming_provider, streamed }.settle(&mut telemetry, &mut guard, Outcome::Succeeded, None).await;
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
                    let settled = AttemptSpan { number, wall_start, id, provider: &streaming_provider, streamed }.settle(&mut telemetry, &mut guard, Outcome::Failed, Some(error)).await;
                    let error = settled.err().unwrap_or(error);
                    telemetry.finish(telemetry::Outcome::Error, Some(error));
                    yield Err(error);
                    break;
                }
            }
        }
    };
    Box::pin(tracked)
}
