//! Settlement of one attempt: the reservation row, usage evidence, valuation
//! under the pinned price, and the ledger entry.
use super::*;
#[derive(sqlx::FromRow)]
pub(super) struct Reservation {
    pub(super) workspace_id: Uuid,
    pub(super) api_key_id: Uuid,
    pub(super) admitted_at: DateTime<Utc>,
    pub(super) deployment_id: Uuid,
    pub(super) price_id: Option<Uuid>,
    pub(super) state: String,
    pub(super) input_tokens: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    pub(super) billing_usage: Option<serde_json::Value>,
    pub(super) workload_kind: String,
    pub(super) reserved_tokens: Option<i64>,
    pub(super) meter_usage: Option<serde_json::Value>,
    pub(super) output_image_variant: Option<String>,
    pub(super) provider_cost_microusd: Option<i64>,
    /// Metrics labels only (provider kind, configured public model).
    pub(super) provider: String,
    pub(super) public_model: String,
    /// Requests covered by one async batch attempt (0016); `None` = one.
    pub(super) request_count: Option<i32>,
    /// Pinned price list (0021): `standard` or `batch`.
    pub(super) price_tier: String,
}
/// A reservation with its execution's labels. Scoped mode (`lock`): locks
/// the reservation row, the serializer of everything that changes one
/// reservation (settlement, reconciliation, realtime windows).
pub(super) async fn reservation(
    tx: &mut Tx<'_>,
    id: Uuid,
    lock: bool,
) -> Result<Reservation, InferenceError> {
    sqlx::query_as(if lock {
        "SELECT r.workspace_id,r.api_key_id,r.admitted_at,r.deployment_id,r.price_id,r.state,r.input_tokens,r.output_tokens,r.billing_usage,e.workload_kind,r.reserved_tokens,r.meter_usage,r.output_image_variant,r.provider_cost_microusd,e.provider,e.public_model,r.request_count,r.price_tier FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=$1 FOR UPDATE OF r"
    } else {
        "SELECT r.workspace_id,r.api_key_id,r.admitted_at,r.deployment_id,r.price_id,r.state,r.input_tokens,r.output_tokens,r.billing_usage,e.workload_kind,r.reserved_tokens,r.meter_usage,r.output_image_variant,r.provider_cost_microusd,e.provider,e.public_model,r.request_count,r.price_tier FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=$1"
    }).bind(id).fetch_one(&mut **tx).await.map_err(storage)
}
impl Reservation {
    /// The totals buckets a change of this reservation touches.
    pub(super) fn touch(&self) -> locks::Touch {
        locks::Touch {
            workspace: self.workspace_id,
            api_key: self.api_key_id,
            at: self.admitted_at,
        }
    }
}
/// Meter, variant and provider-cost evidence as stored columns.
pub(super) struct MeterEvidence {
    pub(super) meters: Option<serde_json::Value>,
    pub(super) variant: Option<String>,
    pub(super) provider_cost: Option<i64>,
}
pub(super) fn meter_evidence(usage: Usage) -> Result<MeterEvidence, InferenceError> {
    if usage
        .meters
        .is_some_and(|m: MeterUsage| m.validate().is_err())
        || usage.provider_cost_microusd.is_some_and(|n| n < 0)
    {
        return Err(InferenceError::Storage);
    }
    Ok(MeterEvidence {
        meters: usage
            .meters
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| InferenceError::Storage)?,
        variant: usage
            .output_image_variant
            .map(|v: MeterVariant| v.as_str().to_owned()),
        provider_cost: usage.provider_cost_microusd,
    })
}
pub(super) fn usage_values(usage: Usage) -> Result<(Option<i64>, Option<i64>), InferenceError> {
    meter_evidence(usage)?;
    validate_token_usage(usage)
}
/// Shared token coherence checks; callers choose provider or operator error semantics.
pub(crate) fn validate_token_usage(
    usage: Usage,
) -> Result<(Option<i64>, Option<i64>), InferenceError> {
    if let Some(b) = usage.billing {
        b.validate().map_err(|_| InferenceError::Storage)?;
        if usage
            .input_tokens
            .zip(b.total_input_tokens)
            .is_some_and(|(raw, total)| raw > total)
        {
            return Err(InferenceError::Storage);
        }
    }
    let input = usage
        .input_tokens
        .map(i64::try_from)
        .transpose()
        .map_err(|_| InferenceError::Storage)?;
    let output = usage
        .output_tokens
        .map(i64::try_from)
        .transpose()
        .map_err(|_| InferenceError::Storage)?;
    // Two individually legal observations must not overflow total-token enforcement.
    if input
        .zip(output)
        .is_some_and(|(a, b)| a.checked_add(b).is_none())
    {
        return Err(InferenceError::Storage);
    }
    Ok((input, output))
}
pub(super) fn billing_json(usage: Usage) -> Result<Option<serde_json::Value>, InferenceError> {
    usage
        .billing
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| InferenceError::Storage)
}
pub(super) fn workload_usage(
    r: &Reservation,
    mut usage: Usage,
    invalid_output: InferenceError,
) -> Result<Usage, InferenceError> {
    // Input-only workloads: output zero is semantic non-applicability.
    // Provider observations and operator input use different refusal semantics.
    if matches!(r.workload_kind.as_str(), "embeddings" | "rerank") {
        if usage.output_tokens.is_some_and(|n| n != 0) {
            return Err(invalid_output);
        }
        usage.output_tokens = Some(0);
    }
    usage_values(usage)?;
    Ok(usage)
}
pub(super) struct Valuation {
    pub(super) actual: Option<i64>,
    pub(super) floor: Option<i64>,
    pub(super) components: Option<CostBreakdown>,
    pub(super) violated: bool,
}
/// A finite hold must cover every category still possible under the inclusive
/// ceiling. Unknown-rate categories are possible whenever capacity remains;
/// not-applicable categories only when observed evidence forces usage into them. Residuals constrain possibilities; they are not observed allocations
/// and must never be fed into settlement or the known monetary floor.
pub(super) fn cache_bound_violated(
    usage: Option<&BillingUsage>,
    rates: &CachePricing,
    input_limit: u64,
) -> Result<bool, billing::BillingError> {
    let b = usage.copied().unwrap_or_default();
    b.validate()?;
    let parts = b.write_parts();
    let known_writes = parts.into_iter().flatten().sum::<u64>();
    let inclusive = b.total_input_tokens.unwrap_or(input_limit);
    let read_capacity = inclusive
        .saturating_sub(b.uncached_input_tokens.unwrap_or(0))
        .saturating_sub(b.cache_write_input_tokens.unwrap_or(0).max(known_writes));
    let write_capacity = b.cache_write_input_tokens.unwrap_or_else(|| {
        inclusive
            .saturating_sub(b.uncached_input_tokens.unwrap_or(0))
            .saturating_sub(b.cache_read_input_tokens.unwrap_or(0))
    });
    let write_residual = write_capacity.saturating_sub(known_writes);
    let rates = rates.rates();
    let observed = [b.cache_read_input_tokens, parts[0], parts[1], parts[2]];
    let capacity = [
        read_capacity,
        write_residual,
        write_residual,
        write_residual,
    ];
    let missing_writes = || (1..4).filter(|&i| observed[i].is_none());
    // `not_applicable` asserts the certified profile cannot produce a category,
    // so an unobserved NA category is possible only when observed counters force
    // a positive residual into it (every slot that could absorb it is NA).
    // Unknown rates stay conservative: any remaining capacity is possible.
    let mut forced = [false; 4];
    let mut residuals = Vec::with_capacity(2);
    if let Some(aggregate) = b.cache_write_input_tokens {
        residuals.push((
            aggregate.saturating_sub(known_writes),
            missing_writes().collect::<Vec<_>>(),
        ));
    }
    if let (Some(total), Some(uncached)) = (b.total_input_tokens, b.uncached_input_tokens) {
        let writes = b.cache_write_input_tokens.unwrap_or(0).max(known_writes);
        let explained = uncached
            .saturating_add(b.cache_read_input_tokens.unwrap_or(0))
            .saturating_add(writes);
        let mut slots = Vec::with_capacity(4);
        if b.cache_read_input_tokens.is_none() {
            slots.push(0);
        }
        if b.cache_write_input_tokens.is_none() {
            slots.extend(missing_writes());
        }
        residuals.push((total.saturating_sub(explained), slots));
    }
    for (residual, slots) in residuals {
        if residual == 0 {
            continue;
        }
        // Contradictory unexplained input is bounded only if every category is priced.
        if slots.is_empty() {
            if rates.iter().any(|r| !matches!(r, CacheRate::Priced { .. })) {
                return Ok(true);
            }
            continue;
        }
        if slots
            .iter()
            .all(|&i| matches!(rates[i], CacheRate::NotApplicable))
        {
            for i in slots {
                forced[i] = true;
            }
        }
    }
    Ok((0..4).any(|i| match (observed[i], rates[i]) {
        (Some(n), CacheRate::Unknown | CacheRate::NotApplicable) => n > 0,
        (None, CacheRate::Unknown) => capacity[i] > 0,
        (None, CacheRate::NotApplicable) => forced[i],
        (_, CacheRate::Priced { .. }) => false,
    }))
}
#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
pub(super) async fn pinned_value(
    tx: &mut Tx<'_>,
    r: &Reservation,
    usage: Usage,
) -> Result<Valuation, InferenceError> {
    let unresolved = |violated| Valuation {
        actual: None,
        floor: None,
        components: None,
        violated,
    };
    let Some(id) = r.price_id else {
        return Ok(unresolved(false));
    };
    let p = price_version(tx, id, r.deployment_id)
        .await?
        .tiered(&r.price_tier)?;
    // Validate observations and rates before distinguishing monetary overflow.
    // BillingError::Overflow can also mean an invalid, unstoreable token count;
    // that must remain an error rather than being accepted as unknown cost.
    usage_values(usage)?;
    let observed_input = usage
        .billing
        .map(|b| b.input_lower_bound())
        .transpose()
        .map_err(|_| InferenceError::Storage)?
        .unwrap_or(0)
        .max(usage.input_tokens.unwrap_or(0));
    enum Rates {
        V1(i64, i64),
        V2(i64, i64, CachePricing),
        V3(PriceLines, MaxUnits),
    }
    let rates = match p.pricing_version {
        1 | 2 => {
            let (i, o) = p.token_rates()?;
            billing::charge(0, i)
                .and_then(|_| billing::charge(0, o))
                .map_err(|_| InferenceError::Storage)?;
            if p.pricing_version == 1 {
                Rates::V1(i, o)
            } else {
                let rates = p.rates()?;
                rates.validate().map_err(|_| InferenceError::Storage)?;
                Rates::V2(i, o, rates)
            }
        }
        3 => {
            let (lines, max) = p.lines()?;
            Rates::V3(lines, max)
        }
        _ => return Err(InferenceError::Storage),
    };
    let priced = (|| -> Result<Valuation, billing::BillingError> {
        let mut value = unresolved(false);
        if let Rates::V3(lines, max) = &rates {
            let v = billing::v3::value(
                lines,
                max,
                &billing::v3::Observed {
                    billing: usage.billing.as_ref(),
                    output_tokens: usage.output_tokens,
                    meters: usage.meters.as_ref(),
                    variant: usage.output_image_variant,
                    // Realtime audio tokens are valued per response in
                    // `governance::realtime`, never here.
                    audio: None,
                    // Only async video jobs can produce video seconds.
                    video: r.workload_kind == WorkloadKind::Videos.as_str(),
                },
            )?;
            value.components = v.components.map(|(c, m)| CostBreakdown::Metered(c, m));
            value.actual = value.components.map(|c| c.total()).transpose()?;
            value.floor = Some(v.floor);
            value.violated = v.violated
                || cache_bound_violated(
                    usage.billing.as_ref(),
                    &billing::v3::cache_rates(lines),
                    p.input_token_limit as u64,
                )?;
        } else if let Rates::V2(i, o, rates) = &rates {
            value.components = usage
                .billing
                .map(|b| billing::value_v2(*i, *o, rates, &b, usage.output_tokens))
                .transpose()?
                .flatten()
                .map(CostBreakdown::Tokens);
            value.actual = value.components.map(|c| c.total()).transpose()?;
            value.floor = Some(billing::floor_v2(
                *i,
                *o,
                rates,
                usage.billing.as_ref(),
                usage.output_tokens,
            )?);
            value.violated =
                cache_bound_violated(usage.billing.as_ref(), rates, p.input_token_limit as u64)?;
        } else if let Rates::V1(i, o) = rates {
            // Normalized metadata supersedes raw (possibly cache-exclusive) input.
            // V1 has one aggregate rate, not a disjoint cache breakdown.
            let input = match usage.billing {
                Some(b) => b.total_input_tokens,
                None => usage.input_tokens,
            };
            value.actual = input
                .zip(usage.output_tokens)
                .map(|(a, b)| checked_cost(a, b, i, o))
                .transpose()?;
            value.floor = Some(checked_cost(
                observed_input,
                usage.output_tokens.unwrap_or(0),
                i,
                o,
            )?);
        }
        Ok(value)
    })();
    let mut value = match priced {
        Ok(value) => value,
        // The counts are valid, but no representable monetary amount can cover
        // them. Keep the hold and persist the observation as unknown/unbounded.
        Err(billing::BillingError::Overflow) => return Ok(unresolved(true)),
        Err(_) => return Err(InferenceError::Storage),
    };
    // An async batch attempt covers `request_count` requests (aggregated usage).
    let input_bound = p
        .input_token_limit
        .checked_mul(i64::from(r.request_count.unwrap_or(1)))
        .ok_or(InferenceError::Storage)?;
    let output_bound = r.reserved_tokens.and_then(|n| n.checked_sub(input_bound));
    value.violated |= observed_input > input_bound as u64
        || output_bound.is_some_and(|limit| usage.output_tokens.is_some_and(|n| n > limit as u64));
    Ok(value)
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn ledger(
    tx: &mut Tx<'_>,
    id: Uuid,
    admitted_at: Option<DateTime<Utc>>,
    kind: &str,
    amount: Option<i64>,
    usage: Usage,
    components: Option<CostBreakdown>,
    evidence: Option<&str>,
) -> Result<(), InferenceError> {
    let (input, output) = usage_values(usage)?;
    let m = meter_evidence(usage)?;
    // admitted_at (0030): the ledger is partitioned with its reservation.
    // A known admission time prunes the reservation lookup to its month.
    let written = sqlx::query(if admitted_at.is_some() {
        "INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,output_image_variant,provider_cost_microusd,admitted_at) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,r.admitted_at FROM governance_reservations r WHERE r.execution_id=$2 AND r.admitted_at=$13"
    } else {
        "INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,output_image_variant,provider_cost_microusd,admitted_at) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,r.admitted_at FROM governance_reservations r WHERE r.execution_id=$2 AND $13::timestamptz IS NULL"
    })
        .bind(Uuid::now_v7()).bind(id).bind(kind).bind(amount).bind(input).bind(output).bind(billing_json(usage)?).bind(components.map(|c|c.to_value())).bind(evidence).bind(m.meters).bind(m.variant).bind(m.provider_cost).bind(admitted_at).execute(&mut **tx).await.map_err(storage)?.rows_affected();
    if written != 1 {
        return Err(InferenceError::Storage);
    }
    Ok(())
}
/// Token meters a pinned v3 price marks `not_applicable` (all input-family
/// meters; output) assert the provider cannot charge them, so unreported
/// counts are semantic zeros (e.g. per-second transcription, per-character
/// speech). Under any other line unreported tokens stay unknown.
async fn not_applicable_tokens(
    tx: &mut Tx<'_>,
    r: &Reservation,
) -> Result<(bool, bool), InferenceError> {
    let Some(lines) = pinned_v3_lines(tx, r).await? else {
        return Ok((false, false));
    };
    Ok((
        lines.input_tokens_inapplicable(),
        lines.not_applicable(billing::v3::Meter::OutputTokens),
    ))
}
/// The pinned price's v3 lines; `None` when unpriced or v1/v2.
async fn pinned_v3_lines(
    tx: &mut Tx<'_>,
    r: &Reservation,
) -> Result<Option<PriceLines>, InferenceError> {
    let Some(id) = r.price_id else {
        return Ok(None);
    };
    let p = price_version(tx, id, r.deployment_id)
        .await?
        .tiered(&r.price_tier)?;
    if p.pricing_version != 3 {
        return Ok(None);
    }
    Ok(Some(p.lines()?.0))
}
/// A failed attempt settles at a known zero only when all of these hold: the
/// provider rejected the request before processing (`upstream_rejected`: a 4xx
/// validation, moderation or filtered/missing-model response), it reported no
/// usage at all (no token, meter or nonzero provider-cost evidence), and every
/// meter of the pinned v3 price is explicitly free or not applicable. Anything
/// else (other failures, partial usage, unpriced/missing/positive meters, v1/v2)
/// stays unknown.
async fn free_rejection(
    tx: &mut Tx<'_>,
    r: &Reservation,
    record: &ExecutionFinish,
) -> Result<bool, InferenceError> {
    let u = record.usage;
    if record.outcome != Outcome::Failed
        || !matches!(
            record.error,
            Some(InferenceError::UpstreamRejected | InferenceError::ContextLength)
        )
        || u.input_tokens.is_some()
        || u.output_tokens.is_some()
        || u.billing.is_some()
        || u.meters.is_some()
        || u.output_image_variant.is_some()
        || u.provider_cost_microusd.is_some_and(|c| c != 0)
    {
        return Ok(false);
    }
    Ok(pinned_v3_lines(tx, r)
        .await?
        .is_some_and(|l| l.all_free_or_not_applicable()))
}
/// Identical terminal finishes include the full normalized breakdown, not just raw counts.
pub async fn finish(store: &Store, record: &ExecutionFinish) -> Result<(), InferenceError> {
    finish_with_telemetry(
        store,
        record,
        &crate::inference::repository::AttemptTelemetry::default(),
    )
    .await
}
/// [`finish`] plus attempt telemetry (finish reason, timings, reasoning
/// tokens), written in the same transaction as the terminal state. Telemetry
/// is metadata only and never affects accounting or idempotency.
pub async fn finish_with_telemetry(
    store: &Store,
    record: &ExecutionFinish,
    telemetry: &crate::inference::repository::AttemptTelemetry,
) -> Result<(), InferenceError> {
    use crate::metrics::{AttemptObservation, METRICS, PhaseTimer, Settlement};
    let mut timer = PhaseTimer::start();
    let result = locks::retry_deadlocks!(
        "settlement",
        finish_unobserved(store, record, telemetry, &mut timer).await
    );
    METRICS.observe_settlement_phases(
        timer,
        match &result {
            Ok(Finished::Replay) => "replay",
            Ok(Finished::Conflict) => "conflict",
            Ok(Finished::Committed { settled: true, .. }) => "settled",
            Ok(Finished::Committed { settled: false, .. }) => "unknown",
            Err(_) => "error",
        },
    );
    match result {
        Ok(Finished::Replay) => Ok(()),
        Ok(Finished::Conflict) => Err(InferenceError::Storage),
        Ok(Finished::Committed {
            provider,
            model,
            settled,
            input,
            output,
        }) => {
            let telemetry = telemetry.for_outcome(record.outcome);
            let labels = METRICS.labels(&provider, &model);
            METRICS.observe_attempt(AttemptObservation {
                labels: &labels,
                outcome: record.outcome.as_str(),
                error: record.error,
                input_tokens: input,
                output_tokens: output,
                generation_ms: telemetry.generation_ms,
                time_to_first_token_ms: telemetry.time_to_first_token_ms,
            });
            METRICS.observe_settlement(if settled {
                Settlement::Settled
            } else {
                Settlement::Unknown
            });
            Ok(())
        }
        Err(error) => {
            // The reservation stays pending (held) until reconciliation.
            METRICS.observe_settlement(Settlement::Held);
            Err(error)
        }
    }
}
/// Metrics view of a terminal finish (labels come from the durable row).
enum Finished {
    /// An identical terminal finish was already recorded.
    Replay,
    /// A different terminal finish was already recorded.
    Conflict,
    Committed {
        provider: String,
        model: String,
        settled: bool,
        input: Option<i64>,
        output: Option<i64>,
    },
}
async fn finish_unobserved(
    store: &Store,
    record: &ExecutionFinish,
    telemetry: &crate::inference::repository::AttemptTelemetry,
    timer: &mut crate::metrics::PhaseTimer,
) -> Result<Finished, InferenceError> {
    let telemetry = telemetry.for_outcome(record.outcome);
    let ms = |v: Option<u64>| v.map(|n| n.min(i64::MAX as u64) as i64);
    let reasoning = record
        .usage
        .reasoning_tokens
        .filter(|n| *n <= i64::MAX as u64)
        .map(|n| n as i64);
    // Provider-reported served model (0013), validated and bounded by type.
    let reported_model = record.usage.reported_model.map(|m| m.as_str().to_owned());
    // What an elapsed settlement deadline was waiting on (#389).
    use crate::db::accounting_failure::{Waiting, settlement_waiting};
    settlement_waiting(Waiting::Queue);
    let _queued = gate(&store.lock_gates.settlement).await;
    timer.phase("queue");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementQueue);
    settlement_waiting(Waiting::Connection);
    let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
    settlement_waiting(Waiting::Database);
    timer.phase("connect");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementConnect);
    settlement_prefix(store, &mut tx).await?;
    let r = reservation(&mut tx, record.id, scoped(store)).await?;
    timer.phase("locks");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementLocks);
    let mut usage = workload_usage(&r, record.usage, InferenceError::Storage)?;
    // A free price's pre-processing rejection processed no tokens: record the
    // semantic zeros a settled row requires (see `free_rejection`).
    let free = free_rejection(&mut tx, &r, record).await?;
    if free {
        usage.input_tokens.get_or_insert(0);
        usage.output_tokens.get_or_insert(0);
    }
    let (input_na, output_na) = not_applicable_tokens(&mut tx, &r).await?;
    if input_na && usage.input_tokens.is_none() && usage.billing.is_none() {
        usage.input_tokens = Some(0);
    }
    if output_na && usage.output_tokens.is_none() {
        usage.output_tokens = Some(0);
    }
    let (input, output) = usage_values(usage)?;
    let billing = billing_json(usage)?;
    let m = meter_evidence(usage)?;
    if r.state != "pending" {
        let same:bool=sqlx::query_scalar("SELECT state=$2 AND error_code IS NOT DISTINCT FROM $3::text AND input_tokens IS NOT DISTINCT FROM $4::bigint AND output_tokens IS NOT DISTINCT FROM $5::bigint AND billing_usage IS NOT DISTINCT FROM $6::jsonb AND meter_usage IS NOT DISTINCT FROM $7::jsonb AND output_image_variant IS NOT DISTINCT FROM $8::text AND provider_cost_microusd IS NOT DISTINCT FROM $9::bigint FROM inference_executions WHERE id=$1")
            .bind(record.id).bind(record.outcome.as_str()).bind(record.error.map(|e|e.code())).bind(input).bind(output).bind(billing).bind(m.meters).bind(m.variant).bind(m.provider_cost).fetch_one(&mut *tx).await.map_err(storage)?;
        return Ok(if same {
            Finished::Replay
        } else {
            Finished::Conflict
        });
    }
    let value = pinned_value(&mut tx, &r, usage).await?;
    // A settled row records observed token counts; unreported tokens (other
    // than not-applicable ones above) keep the settlement unknown. A free
    // price's pre-processing rejection without usage is a known zero.
    let actual = if record.outcome == Outcome::Succeeded && input.is_some() && output.is_some() {
        value.actual
    } else if free && !value.violated {
        value.actual.filter(|a| *a == 0)
    } else {
        None
    };
    let components = value.components.filter(|_| actual.is_some());
    timer.phase("read");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementRead);
    // Scoped: the reservation's totals rows, held from here to commit (its
    // triggers then take the counter rows in canonical order).
    lock_rows(store, &mut tx, &[r.touch()], false).await?;
    timer.phase("lock_rows");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementTotalsLocks);
    // One statement (one round trip while the totals rows are held): the
    // execution's outcome, the reservation's settlement and the ledger
    // entry. Each names the partition key (started_at = admitted_at,
    // enforced by the reservation -> execution key), so it touches one month
    // partition. The reservation and ledger writes run only when the
    // execution was still running; anything else fails and rolls back.
    let (executions, reservations, entries): (i64, i64, i64) = sqlx::query_as("WITH e AS (UPDATE inference_executions SET state=$2,error_code=$3,input_tokens=$4,output_tokens=$5,billing_usage=$6,elapsed_ms=$7,completed_at=clock_timestamp(),meter_usage=$8,output_image_variant=$9,provider_cost_microusd=$10,finish_reason=$11,time_to_first_token_ms=$12,generation_ms=$13,reasoning_tokens=$14,reported_upstream_model=$15 WHERE id=$1 AND started_at=$16 AND state='started' RETURNING 1),
      r AS (UPDATE governance_reservations SET state=$17,actual_microusd=$18,input_tokens=$4,output_tokens=$5,billing_usage=$6,cost_components=$19,held_microusd=CASE WHEN $20::bigint IS NULL THEN held_microusd ELSE greatest(held_microusd,$20) END,unbounded_cost=unbounded_cost OR $21,meter_usage=$8,output_image_variant=$9,provider_cost_microusd=$10 WHERE execution_id=$1 AND admitted_at=$16 AND EXISTS(SELECT 1 FROM e) RETURNING 1),
      l AS (INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,output_image_variant,provider_cost_microusd,admitted_at) SELECT $22,$1,$23,$24,$4,$5,$6,$19,NULL,$8,$9,$10,$16 WHERE EXISTS(SELECT 1 FROM r) RETURNING 1)
      SELECT (SELECT count(*) FROM e),(SELECT count(*) FROM r),(SELECT count(*) FROM l)")
        .bind(record.id).bind(record.outcome.as_str()).bind(record.error.map(|e|e.code())).bind(input).bind(output).bind(&billing).bind(record.elapsed_ms.min(i64::MAX as u64)as i64).bind(&m.meters).bind(&m.variant).bind(m.provider_cost)
        .bind(telemetry.finish_reason.map(|f|f.as_str())).bind(ms(telemetry.time_to_first_token_ms)).bind(ms(telemetry.generation_ms)).bind(reasoning).bind(&reported_model).bind(r.admitted_at)
        .bind(if actual.is_some(){"settled"}else{"unknown"}).bind(actual).bind(components.map(|c|c.to_value())).bind(value.floor.filter(|_|actual.is_none())).bind(value.violated)
        .bind(Uuid::now_v7()).bind(if actual.is_some(){"settlement"}else{"unknown"}).bind(actual.or(value.floor))
        .fetch_one(&mut *tx).await.map_err(storage)?;
    if (executions, reservations, entries) != (1, 1, 1) {
        return Err(InferenceError::Storage);
    }
    timer.phase("write");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementWrite);
    tx.commit().await.map_err(storage)?;
    timer.phase("commit");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::SettlementCommit);
    Ok(Finished::Committed {
        provider: r.provider,
        model: r.public_model,
        settled: actual.is_some(),
        input,
        output,
    })
}
