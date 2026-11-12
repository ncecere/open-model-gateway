//! Admission entry points, reservation bounds and limit enforcement. The
//! admission transaction itself (`admit_unobserved`) is in the parent module.
use super::*;
fn generation(max_output_tokens: Option<u32>) -> WorkloadAdmission {
    WorkloadAdmission {
        kind: WorkloadKind::Generation,
        output: OutputReservation::Requested(max_output_tokens),
        unit_ceilings: MeterUsage::default(),
    }
}
pub async fn admit(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
) -> Result<(), InferenceError> {
    admit_checked(
        store,
        record,
        generation(request.max_output_tokens),
        lease_seconds,
        None,
        u32::MAX,
        Some(request),
    )
    .await
    .map(|_| ())
}
pub async fn admit_for_deployment(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
    deployment: &Deployment,
) -> Result<(), InferenceError> {
    admit_generation(store, record, request, lease_seconds, deployment, u32::MAX)
        .await
        .map(|_| ())
}
/// Resolve and reserve the exact output bound before any execution writes.
pub async fn admit_generation(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
    deployment: &Deployment,
    output_limit: u32,
) -> Result<crate::inference::repository::GenerationAdmission, InferenceError> {
    admit_checked(
        store,
        record,
        generation(request.max_output_tokens),
        lease_seconds,
        Some(deployment),
        output_limit,
        Some(request),
    )
    .await
}
/// Only priced omissions need a server default. Explicit invalid values are
/// client errors; an unusable server default is a configuration error.
pub(crate) fn generation_bound(
    price_ceiling: Option<i64>,
    requested: Option<u32>,
    output_limit: u32,
) -> Result<crate::inference::repository::GenerationAdmission, InferenceError> {
    if price_ceiling.is_some_and(|n| n < 1) {
        return Err(InferenceError::Configuration);
    }
    let max_output_tokens = match requested {
        Some(n) => {
            if n == 0
                || n > output_limit
                || price_ceiling.is_some_and(|ceiling| i64::from(n) > ceiling)
            {
                return Err(InferenceError::InvalidRequest);
            }
            Some(n)
        }
        None => price_ceiling
            .map(|ceiling| {
                u32::try_from(ceiling)
                    .ok()
                    .filter(|n| *n > 0 && *n <= output_limit)
                    .ok_or(InferenceError::Configuration)
            })
            .transpose()?,
    };
    Ok(crate::inference::repository::GenerationAdmission { max_output_tokens })
}
pub async fn admit_embeddings_for_deployment(
    store: &Store,
    record: &ExecutionStart,
    _request: &EmbeddingRequest,
    lease_seconds: i64,
    deployment: &Deployment,
) -> Result<(), InferenceError> {
    admit_checked(
        store,
        record,
        WorkloadAdmission {
            kind: WorkloadKind::Embeddings,
            output: OutputReservation::None,
            unit_ceilings: MeterUsage::default(),
        },
        lease_seconds,
        Some(deployment),
        u32::MAX,
        None,
    )
    .await
    .map(|_| ())
}
/// Generic non-generation admission: the deployment must still declare a
/// protocol of this workload, and v3 holds include meter bounds.
pub async fn admit_workload_for_deployment(
    store: &Store,
    record: &ExecutionStart,
    admission: &WorkloadAdmission,
    lease_seconds: i64,
    deployment: &Deployment,
) -> Result<(), InferenceError> {
    if admission.kind == WorkloadKind::Generation {
        return Err(InferenceError::Configuration);
    }
    admit_checked(
        store,
        record,
        *admission,
        lease_seconds,
        Some(deployment),
        u32::MAX,
        None,
    )
    .await
    .map(|_| ())
}
async fn admit_checked(
    store: &Store,
    record: &ExecutionStart,
    workload: WorkloadAdmission,
    lease_seconds: i64,
    expected: Option<&Deployment>,
    output_limit: u32,
    generation_request: Option<&ChatRequest>,
) -> Result<crate::inference::repository::GenerationAdmission, InferenceError> {
    let mut timer = crate::metrics::PhaseTimer::start();
    let result = locks::retry_deadlocks!(
        "admission",
        admit_unobserved(
            store,
            record,
            workload,
            lease_seconds,
            expected,
            output_limit,
            generation_request,
            &mut timer
        )
        .await
    );
    let observed = result.as_ref().map(|_| ()).map_err(|e| *e);
    crate::metrics::observe_admission(&observed);
    crate::metrics::METRICS
        .observe_admission_phases(timer, crate::metrics::admission_outcome(&observed));
    result
}
/// Token reservation and monetary hold of one attempt under `price` (none
/// without a price): pure arithmetic over the immutable price row.
pub(super) fn reservation_bounds(
    price: Option<&Price>,
    workload: &WorkloadAdmission,
) -> Result<(Option<i64>, Option<i64>), InferenceError> {
    Ok(if let Some(p) = price {
        let output = match workload.output {
            OutputReservation::None => 0,
            OutputReservation::Requested(max) => i64::from(
                max.filter(|n| *n > 0)
                    .ok_or(InferenceError::Configuration)?,
            ),
            OutputReservation::PriceCeiling => p.output_token_limit,
            // Every line maximum is checked below; the sum is the ceiling.
            OutputReservation::Batch {
                requests,
                output_tokens,
                max_line_output,
            } => {
                if requests == 0
                    || max_line_output == 0
                    || i64::from(max_line_output) > p.output_token_limit
                    || output_tokens < u64::from(max_line_output)
                    || output_tokens > u64::from(requests) * u64::from(max_line_output)
                {
                    return Err(InferenceError::Configuration);
                }
                i64::try_from(output_tokens).map_err(|_| InferenceError::Configuration)?
            }
        };
        let requests = match workload.output {
            OutputReservation::Batch { requests, .. } => i64::from(requests),
            _ => 1,
        };
        // A zero input ceiling is valid only for v3 prices whose input-family
        // token meters are all not applicable (validated again here).
        let zero_input_ok = p.input_token_limit == 0
            && p.pricing_version == 3
            && p.lines()?.0.input_tokens_inapplicable();
        if (p.input_token_limit < 1 && !zero_input_ok)
            || (output > p.output_token_limit && requests == 1)
        {
            return Err(InferenceError::Configuration);
        }
        // A realtime session holds one minimal response window, resized from
        // the session's context by its first `response.create` (`realtime`).
        let realtime = (workload.kind == WorkloadKind::Realtime).then(|| {
            crate::inference::realtime::ResponseBound::admission(
                u32::try_from(output).unwrap_or(u32::MAX),
            )
        });
        (
            Some(match realtime {
                Some(window) => p.realtime_tokens(window)?,
                None => p
                    .input_token_limit
                    .checked_mul(requests)
                    .and_then(|n| n.checked_add(output))
                    .ok_or(InferenceError::Configuration)?,
            }),
            if let Some(window) = realtime {
                p.realtime_window(window)?
            } else if matches!(workload.kind, WorkloadKind::Videos | WorkloadKind::Batches) {
                p.bound_scaled(
                    requests as u64,
                    output as u64,
                    &workload.unit_ceilings,
                    workload.kind == WorkloadKind::Videos,
                )?
            } else {
                p.bound(output as u64, &workload.unit_ceilings)?
            },
        )
    } else {
        (None, None)
    })
}
/// Which limits an admission is subject to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LimitMode {
    /// Interactive requests: every rate and concurrency limit, never jobs.
    Interactive,
    /// Async jobs (video, batch): "jobs at once" and "requests at once" only.
    Job,
    /// A gateway-run batch line (0021): its batch already holds the job slot
    /// and ceiling, so only budgets apply (to the part of the hold its batch
    /// does not cover).
    BatchLine,
}
/// Evaluate every applicable policy layer and budget of the workspace and key
/// lineage (no installation layer) under the installation lock and return the most actionable denial: budget/accounting (narrowest
/// scope first) before the job limit before retryable rate limits.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enforce_limits(
    tx: &mut Tx<'_>,
    workspace: Uuid,
    lineage: Uuid,
    now: DateTime<Utc>,
    mode: LimitMode,
    tokens: Option<i64>,
    held: Option<i64>,
    v3_unbounded: bool,
) -> Result<(), InferenceError> {
    let job = mode == LimitMode::Job;
    // Policies, budgets with their maintained totals, and the workspace/key
    // rate counters in one round trip.
    let rates::LimitState {
        policies,
        budgets,
        counters,
    } = rates::read_limits(tx, workspace, lineage, now, mode != LimitMode::BatchLine)
        .await
        .map_err(storage)?;
    if policies
        .iter()
        .any(|p| p.tokens_per_minute.is_some() && tokens.is_none() && !job)
        || (!budgets.is_empty() && held.is_none() && !v3_unbounded)
    {
        return Err(InferenceError::Configuration);
    }
    // No budget can be enforced against an unbounded hold. This is the
    // price's configuration, not any scope's spending: never `budget_exceeded`.
    if !budgets.is_empty() && v3_unbounded {
        return Err(InferenceError::PriceUnbounded);
    }
    // Evaluate every applicable layer, then report the most actionable denial:
    // budget/accounting (narrowest scope first) before retryable rate limits.
    // Budget/accounting denials rank above the job limit, which ranks above
    // other retryable rate limits.
    let rank = |scope| match scope {
        LimitScope::ApiKey => 4,
        LimitScope::Workspace => 3,
    };
    let mut denial: Option<(u8, InferenceError)> = None;
    let mut deny = |found: (u8, InferenceError)| {
        if denial.is_none_or(|d| found.0 > d.0) {
            denial = Some(found);
        }
    };
    // Rate layers: the maintained counters of the current UTC minute and live
    // leases (O(scopes), not O(rows)) of the workspace and the key lineage.
    for p in policies {
        let scope = LimitScope::of(p.api_key_id);
        // Jobs are exempt from per-minute limits; interactive work never
        // checks the job limit.
        let (requests_per_minute, tokens_per_minute, concurrent_jobs) = if job {
            (None, None, p.concurrent_jobs)
        } else {
            (p.requests_per_minute, p.tokens_per_minute, None)
        };
        // A reservation that alone exceeds a tokens-per-minute limit can never
        // be admitted, however idle the minute is: report it honestly (scope
        // kind only, never amounts) instead of a transient rate limit.
        if tokens_per_minute
            .zip(tokens)
            .is_some_and(|(limit, reserved)| reserved > limit)
        {
            deny((
                rank(scope),
                InferenceError::TokenReservationExceedsLimit(scope),
            ));
            continue;
        }
        if requests_per_minute.is_none()
            && tokens_per_minute.is_none()
            && p.concurrent_requests.is_none()
            && concurrent_jobs.is_none()
        {
            continue;
        }
        let limits = rates::Limits {
            requests_per_minute,
            tokens_per_minute,
            concurrent_requests: p.concurrent_requests,
            concurrent_jobs,
        };
        let (rate_ok, jobs_ok) = counters
            .iter()
            .find(|(lineage, _)| *lineage == p.api_key_id)
            .ok_or(InferenceError::Storage)?
            .1
            .admits(limits, tokens);
        if !jobs_ok {
            deny((1, InferenceError::JobLimitExceeded(scope)));
        }
        if !rate_ok {
            deny((0, InferenceError::Busy));
        }
    }
    // Budgets: every (scope, period) budget read its maintained totals row
    // (same statement), exactly the former window scan.
    let new_hold = i128::from(held.unwrap_or(0));
    for (b, c) in budgets.iter() {
        let scope = LimitScope::of(b.api_key_id);
        if c.unresolved {
            deny((rank(scope), InferenceError::UnresolvedUsage(scope)));
        } else if c.used_microusd + new_hold > i128::from(b.amount_microusd) {
            deny((rank(scope), InferenceError::BudgetExceeded(scope)));
        }
    }
    match denial {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}
