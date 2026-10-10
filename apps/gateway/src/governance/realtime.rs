//! Realtime session accounting. A session is one upstream attempt with one
//! reservation; its hold is a sum of bounded *response windows*, each sized
//! from the session's actual context ([`ResponseBound`], capped at the
//! price's input ceiling as the context window):
//!
//! - admission (shared `admit_workload_for_deployment`) holds one minimal
//!   window ([`ResponseBound::admission`]);
//! - [`reserve_window`] resizes the reserved unused window, or adds one,
//!   before a `response.create`, under live key/model authorization and every
//!   applicable budget (read from the maintained totals at the session's
//!   admission time) and tokens-per-minute limit (in the session's admission
//!   minute);
//! - [`settle_response`] replaces a response's window with its pinned-price
//!   value, or keeps `max(window, known floor)` when usage is unknown;
//! - [`finish`] settles the reservation when every response settled
//!   (actual = Σ responses) and otherwise leaves it unknown with the retained
//!   holds. An unused reserved window is released.
//!
//! All writes go through `governance_reservations`, so the budget-total
//! triggers (0015) stay exact. Response rows (`realtime_responses`, 0017) hold
//! metadata only.
use super::*;
use crate::{
    auth::Principal,
    billing::{
        CostBreakdown,
        v3::{AudioTokens, Meter, Observed},
    },
    inference::realtime::{RealtimeFinish, RealtimeUsage, ResponseBound, ResponseStatus},
};
use chrono::{DateTime, Utc};

impl Price {
    /// `(text input, audio input, output)` ceilings of a window: each input
    /// modality capped at the input ceiling (the model's context window).
    fn realtime_ceilings(&self, b: ResponseBound) -> Result<(u64, u64, u64), InferenceError> {
        let context =
            u64::try_from(self.input_token_limit).map_err(|_| InferenceError::Configuration)?;
        Ok(b.ceilings(context))
    }
    /// Tokens a window reserves against tokens-per-minute limits: its input
    /// (at most the context window) plus its output.
    pub(super) fn realtime_tokens(&self, b: ResponseBound) -> Result<i64, InferenceError> {
        let (text, audio, output) = self.realtime_ceilings(b)?;
        let context = u64::try_from(self.input_token_limit).unwrap_or(0);
        i64::try_from(text.saturating_add(audio).min(context) + output)
            .map_err(|_| InferenceError::Configuration)
    }
    /// Hold of one realtime response window: pricing v3 only (audio tokens
    /// have no v1/v2 rate). Text and audio input meters are bounded by the
    /// window's per-modality input ceilings, output by its output tokens,
    /// `requests` by one; meters a realtime response never reports (cache
    /// writes, images, characters, audio seconds, search units) are impossible.
    pub(super) fn realtime_window(&self, b: ResponseBound) -> Result<Option<i64>, InferenceError> {
        if self.pricing_version != 3 {
            return Err(InferenceError::Configuration);
        }
        let (text_input, audio_input, output) = self.realtime_ceilings(b)?;
        let (lines, mut max) = self.lines()?;
        for meter in Meter::ALL.into_iter().filter(|m| !m.is_token()) {
            if meter == Meter::Requests {
                // A request ceiling tightens `max_units`, never loosens it.
                if max.0.get(&meter).is_none_or(|m| *m >= 1) {
                    max.0.insert(meter, 1);
                } else {
                    max.0.remove(&meter);
                }
            } else {
                max.0.insert(meter, 0);
            }
        }
        billing::v3::bound_realtime(
            &lines,
            &max,
            billing::v3::RealtimeCeilings {
                text_input,
                audio_input,
                output,
            },
        )
        .map_err(|_| InferenceError::Configuration)
    }
}

/// The per-response output window: `cap`, lowered to the latest price's
/// output ceiling (an unpriced deployment keeps `cap`).
pub async fn window_output(
    store: &Store,
    deployment: Uuid,
    cap: u32,
) -> Result<u32, InferenceError> {
    let limit: Option<i64> = sqlx::query_scalar("SELECT output_token_limit FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1")
        .bind(deployment)
        .fetch_optional(&store.pool)
        .await
        .map_err(storage)?;
    let window = limit.map_or(i64::from(cap), |l| l.min(i64::from(cap)));
    u32::try_from(window)
        .ok()
        .filter(|w| *w >= 1)
        .ok_or(InferenceError::Configuration)
}

#[derive(sqlx::FromRow)]
struct Session {
    workspace_id: Uuid,
    api_key_id: Uuid,
    deployment_id: Uuid,
    price_id: Option<Uuid>,
    state: String,
    admitted_at: DateTime<Utc>,
    minute_start: DateTime<Utc>,
    held_microusd: Option<i64>,
    unbounded_cost: bool,
    workload_kind: String,
    public_model: String,
}
async fn session(tx: &mut Tx<'_>, id: Uuid) -> Result<Session, InferenceError> {
    let s: Session = sqlx::query_as("SELECT r.workspace_id,r.api_key_id,r.deployment_id,r.price_id,r.state,r.admitted_at,r.minute_start,r.held_microusd,r.unbounded_cost,e.workload_kind,e.public_model FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=$1 FOR UPDATE OF r")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)?
        .ok_or(InferenceError::Storage)?;
    if s.workload_kind != WorkloadKind::Realtime.as_str() {
        return Err(InferenceError::Storage);
    }
    Ok(s)
}
async fn pinned(tx: &mut Tx<'_>, s: &Session) -> Result<Option<Price>, InferenceError> {
    let Some(id) = s.price_id else {
        return Ok(None);
    };
    sqlx::query_as::<_, Price>(&format!(
        "SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE id=$1 AND deployment_id=$2"
    ))
    .bind(id)
    .bind(s.deployment_id)
    .fetch_one(&mut **tx)
    .await
    .map(Some)
    .map_err(storage)
}
fn rank(scope: LimitScope) -> u8 {
    match scope {
        LimitScope::ApiKey => 3,
        LimitScope::Workspace => 2,
    }
}

/// Reserve window `window` before a `response.create`: resize the reserved
/// unused window `armed`, or add one when there is none. Only growth is
/// budget- and rate-checked (shrinking releases). Denials are the admission
/// errors of the same scope.
pub async fn reserve_window(
    store: &Store,
    principal: &Principal,
    id: Uuid,
    model: &str,
    window: ResponseBound,
    armed: Option<ResponseBound>,
) -> Result<(), InferenceError> {
    let _queued = gate(&store.lock_gates.admission).await;
    locks::retry_deadlocks!(
        "admission",
        reserve_window_once(store, principal, id, model, window, armed).await
    )
}
async fn reserve_window_once(
    store: &Store,
    principal: &Principal,
    id: Uuid,
    model: &str,
    window: ResponseBound,
    armed: Option<ResponseBound>,
) -> Result<(), InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    admission_prefix(store, &mut tx, principal).await?;
    let lineage = authorize(store, &mut tx, principal).await?;
    let s = session(&mut tx, id).await?;
    if s.workspace_id != principal.workspace_id
        || s.api_key_id != principal.key_id
        || s.public_model != model
        || s.state != "pending"
    {
        return Err(InferenceError::Storage);
    }
    // Each new window is new work: the model must still be authorized.
    let allowed: Option<Uuid> = sqlx::query_scalar(&format!("SELECT d.id FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$2 AND m.public_name=$3 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled AND 'realtime'=ANY(m.supported_protocols) AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$4) OR EXISTS(SELECT 1 FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$4 AND model_id=m.id)){}", catalog_share(store)))
        .bind(s.workspace_id).bind(s.deployment_id).bind(model).bind(lineage)
        .fetch_optional(&mut *tx).await.map_err(storage)?;
    if allowed.is_none() {
        return Err(InferenceError::ModelUnavailable);
    }
    // `hold`: the change of the session hold (`None`: unbounded); `tokens`:
    // the new window's tokens and the change of reserved tokens.
    let pinned_price = pinned(&mut tx, &s).await?;
    let priced = pinned_price.is_some();
    let (hold, tokens) = match pinned_price {
        Some(p) => {
            let (old_hold, old_tokens) = match armed {
                Some(a) => (p.realtime_window(a)?, p.realtime_tokens(a)?),
                None => (Some(0), 0),
            };
            let new_tokens = p.realtime_tokens(window)?;
            (
                p.realtime_window(window)?
                    .zip(old_hold)
                    .map(|(new, old)| new - old),
                Some((new_tokens, new_tokens - old_tokens)),
            )
        }
        None => (None, None),
    };
    let grows = hold.is_none_or(|h| h > 0) || tokens.is_some_and(|(_, d)| d > 0);
    // Scoped: the session's totals and minute rows (its admission buckets)
    // before reading them; every admission in that minute locks them too.
    lock_rows(
        store,
        &mut tx,
        &[locks::Touch {
            workspace: s.workspace_id,
            api_key: s.api_key_id,
            at: s.admitted_at,
        }],
        true,
    )
    .await?;
    let policies = sqlx::query_as::<_, Policy>(POLICIES)
        .bind(s.workspace_id)
        .bind(lineage)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
    let budgets = sqlx::query_as::<_, Budget>(BUDGETS)
        .bind(s.workspace_id)
        .bind(lineage)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
    let mut denial: Option<(u8, InferenceError)> = None;
    let mut deny = |found: (u8, InferenceError)| {
        if denial.is_none_or(|d| found.0 > d.0) {
            denial = Some(found);
        }
    };
    // Tokens-per-minute: the session's whole reservation counts in its
    // admission minute, so a session can never reserve more than the limit.
    for p in policies.iter().filter(|_| grows) {
        let Some(limit) = p.tokens_per_minute else {
            continue;
        };
        let scope = LimitScope::of(p.api_key_id);
        let Some((window_tokens, tokens)) = tokens else {
            return Err(InferenceError::Configuration);
        };
        if window_tokens > limit {
            deny((
                rank(scope),
                InferenceError::TokenReservationExceedsLimit(scope),
            ));
            continue;
        }
        // Async jobs are exempt from per-minute limits (0018) and never count here.
        let fits: bool = sqlx::query_scalar("SELECT count(*) FILTER(WHERE reserved_tokens IS NULL)=0 AND coalesce(sum(reserved_tokens),0)+$4::bigint<=$5 FROM governance_reservations r WHERE minute_start=$3 AND NOT EXISTS(SELECT 1 FROM inference_executions e WHERE e.id=r.execution_id AND e.workload_kind IN('videos','batches')) AND workspace_id=$1 AND ($2::uuid IS NULL OR api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2))")
            .bind(s.workspace_id).bind(p.api_key_id).bind(s.minute_start).bind(tokens).bind(limit)
            .fetch_one(&mut *tx).await.map_err(storage)?;
        if !fits {
            deny((0, InferenceError::Busy));
        }
    }
    // A pinned price that cannot bound the new window can never be checked
    // against a budget: the price's configuration, not a budget denial.
    if grows && priced && hold.is_none() && !budgets.is_empty() {
        return Err(InferenceError::PriceUnbounded);
    }
    // Budgets: the session's charges belong to the windows containing its
    // admission time (the same buckets the totals triggers use).
    let mut windows = Vec::with_capacity(budgets.len());
    for b in &budgets {
        let period = BudgetPeriod::parse(&b.period).ok_or(InferenceError::Storage)?;
        windows.push((totals::Scope::of(s.workspace_id, b.api_key_id), period));
    }
    let consumption = totals::read(&mut tx, &windows, s.admitted_at)
        .await
        .map_err(storage)?;
    for (b, c) in budgets.iter().zip(consumption).filter(|_| grows) {
        let scope = LimitScope::of(b.api_key_id);
        if c.unresolved {
            deny((rank(scope), InferenceError::UnresolvedUsage(scope)));
        } else if s.unbounded_cost
            || s.held_microusd.is_none()
            || hold.is_none_or(|h| c.used_microusd + i128::from(h) > i128::from(b.amount_microusd))
        {
            deny((rank(scope), InferenceError::BudgetExceeded(scope)));
        }
    }
    if let Some((_, error)) = denial {
        return Err(error);
    }
    let changed = sqlx::query("UPDATE governance_reservations SET held_microusd=held_microusd+$2,reserved_tokens=reserved_tokens+$3,unbounded_cost=unbounded_cost OR $4 WHERE execution_id=$1 AND state='pending'")
        .bind(id).bind(hold.unwrap_or(0)).bind(tokens.map_or(0, |(_, d)| d)).bind(hold.is_none())
        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    tx.commit().await.map_err(storage)
}

/// A reserved window now belongs to response `sequence` (from 1).
pub async fn open_response(
    store: &Store,
    id: Uuid,
    sequence: i32,
    window: ResponseBound,
) -> Result<(), InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    let s = session(&mut tx, id).await?;
    if s.state != "pending" {
        return Err(InferenceError::Storage);
    }
    let hold = match pinned(&mut tx, &s).await? {
        Some(p) => p.realtime_window(window)?,
        None => None,
    };
    sqlx::query("INSERT INTO realtime_responses(execution_id,sequence,window_hold_microusd,unbounded_cost) VALUES($1,$2,$3,$4)")
        .bind(id).bind(sequence).bind(hold).bind(hold.is_none())
        .execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)
}

/// Pinned-price value of one response: `(actual, known floor, violated,
/// components)`. Only v3 prices value realtime audio tokens.
fn value_response(
    p: &Price,
    u: RealtimeUsage,
    window: ResponseBound,
) -> Result<(Option<i64>, i64, bool, Option<CostBreakdown>), InferenceError> {
    if p.pricing_version != 3 || !u.valid() {
        return Ok((None, 0, true, None));
    }
    let (lines, max) = p.lines()?;
    let (text_limit, audio_limit, output_limit) = p
        .realtime_ceilings(window)
        .map_err(|_| InferenceError::Storage)?;
    let billing = BillingUsage {
        total_input_tokens: Some(u.input_text_tokens),
        uncached_input_tokens: Some(u.input_text_tokens - u.cached_text_tokens),
        cache_read_input_tokens: Some(u.cached_text_tokens),
        cache_write_input_tokens: Some(0),
        cache_write_default_input_tokens: Some(0),
        cache_write_5m_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
    };
    let meters = MeterUsage {
        output_images: Some(0),
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some(1),
        ..MeterUsage::default()
    };
    let priced = billing::v3::value(
        &lines,
        &max,
        &Observed {
            billing: Some(&billing),
            output_tokens: Some(u.output_text_tokens),
            meters: Some(&meters),
            audio: Some(AudioTokens {
                input: Some(u.input_audio_tokens - u.cached_audio_tokens),
                cache_read: Some(u.cached_audio_tokens),
                output: Some(u.output_audio_tokens),
            }),
            ..Observed::default()
        },
    )
    .and_then(|v| {
        let violated = v.violated
            || cache_bound_violated(
                Some(&billing),
                &billing::v3::cache_rates(&lines),
                text_limit,
            )?;
        let components = v
            .components
            .map(|(c, m)| CostBreakdown::Realtime(c, m, v.audio));
        let actual = components.map(|c| c.total()).transpose()?;
        Ok((actual, v.floor, violated, components))
    });
    let (actual, floor, violated, components) = match priced {
        Ok(v) => v,
        // Valid counts with no representable charge: unknown and unbounded.
        Err(billing::BillingError::Overflow) => return Ok((None, 0, true, None)),
        Err(_) => return Err(InferenceError::Storage),
    };
    // Above its window's per-modality ceilings, the hold no longer proves an
    // upper bound for this response.
    let violated = violated
        || u.input_text_tokens > text_limit
        || u.input_audio_tokens > audio_limit
        || u.output_tokens() > output_limit;
    Ok((actual, floor, violated, components))
}

/// Settle response `sequence` from `response.done`. Unknown usage (or an
/// unpriced deployment) retains the window; known usage replaces it.
pub async fn settle_response(
    store: &Store,
    id: Uuid,
    sequence: i32,
    status: Option<ResponseStatus>,
    usage: Option<RealtimeUsage>,
    window: ResponseBound,
) -> Result<(), InferenceError> {
    let _queued = gate(&store.lock_gates.settlement).await;
    locks::retry_deadlocks!(
        "settlement",
        settle_response_once(store, id, sequence, status, usage, window).await
    )
}
async fn settle_response_once(
    store: &Store,
    id: Uuid,
    sequence: i32,
    status: Option<ResponseStatus>,
    usage: Option<RealtimeUsage>,
    window: ResponseBound,
) -> Result<(), InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    settlement_prefix(store, &mut tx).await?;
    let s = session(&mut tx, id).await?;
    if s.state != "pending" {
        return Err(InferenceError::Storage);
    }
    let row: Option<(String, Option<i64>)> = sqlx::query_as("SELECT state,window_hold_microusd FROM realtime_responses WHERE execution_id=$1 AND sequence=$2 FOR UPDATE")
        .bind(id).bind(sequence).fetch_optional(&mut *tx).await.map_err(storage)?;
    let Some((state, held)) = row else {
        return Err(InferenceError::Storage);
    };
    if state != "pending" {
        return Err(InferenceError::Storage);
    }
    let (actual, floor, violated, components) = match (pinned(&mut tx, &s).await?, usage) {
        (Some(p), Some(u)) => value_response(&p, u, window)?,
        (Some(_), None) => (None, 0, false, None),
        (None, _) => (None, 0, true, None),
    };
    let delta = held.map(|h| match actual {
        Some(a) => a - h,
        None => floor.max(h) - h,
    });
    let count = |f: fn(&RealtimeUsage) -> u64| usage.map(|u| f(&u) as i64);
    lock_rows(
        store,
        &mut tx,
        &[locks::Touch {
            workspace: s.workspace_id,
            api_key: s.api_key_id,
            at: s.admitted_at,
        }],
        false,
    )
    .await?;
    sqlx::query("UPDATE realtime_responses SET state=$3,status=$4,actual_microusd=$5,floor_microusd=$6,unbounded_cost=unbounded_cost OR $7,input_text_tokens=$8,cached_text_tokens=$9,input_audio_tokens=$10,cached_audio_tokens=$11,output_text_tokens=$12,output_audio_tokens=$13,cost_components=$14,completed_at=clock_timestamp() WHERE execution_id=$1 AND sequence=$2")
        .bind(id).bind(sequence)
        .bind(if actual.is_some() { "settled" } else { "unknown" })
        .bind(status.map(ResponseStatus::as_str))
        .bind(actual).bind((actual.is_none() && usage.is_some()).then_some(floor))
        .bind(violated)
        .bind(count(|u| u.input_text_tokens)).bind(count(|u| u.cached_text_tokens))
        .bind(count(|u| u.input_audio_tokens)).bind(count(|u| u.cached_audio_tokens))
        .bind(count(|u| u.output_text_tokens)).bind(count(|u| u.output_audio_tokens))
        .bind(components.map(|c| c.to_value()))
        .execute(&mut *tx).await.map_err(storage)?;
    let changed = sqlx::query("UPDATE governance_reservations SET held_microusd=held_microusd+$2,unbounded_cost=unbounded_cost OR $3 WHERE execution_id=$1 AND state='pending'")
        .bind(id).bind(delta.unwrap_or(0)).bind(violated || held.is_none())
        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    tx.commit().await.map_err(storage)
}

#[derive(sqlx::FromRow)]
struct Totals {
    responses: i64,
    settled: i64,
    actual: i64,
    retained: Option<i64>,
    known_floor: i64,
    unbounded: bool,
    measured: i64,
    input: i64,
    output: i64,
    components: Option<serde_json::Value>,
}

/// Finish the session attempt from its response rows (see module docs).
pub async fn finish(store: &Store, record: &RealtimeFinish) -> Result<(), InferenceError> {
    use crate::metrics::{AttemptObservation, METRICS, Settlement};
    let result = finish_unobserved(store, record).await;
    match &result {
        Ok((provider, model, settled, input, output)) => {
            let telemetry = record.telemetry.for_outcome(record.outcome);
            let labels = METRICS.labels(provider, model);
            METRICS.observe_attempt(AttemptObservation {
                labels: &labels,
                outcome: record.outcome.as_str(),
                error: record.error,
                input_tokens: *input,
                output_tokens: *output,
                generation_ms: telemetry.generation_ms,
                time_to_first_token_ms: telemetry.time_to_first_token_ms,
            });
            METRICS.observe_settlement(if *settled {
                Settlement::Settled
            } else {
                Settlement::Unknown
            });
        }
        Err(_) => METRICS.observe_settlement(Settlement::Held),
    }
    result.map(|_| ())
}
type Finished = (String, String, bool, Option<i64>, Option<i64>);
async fn finish_unobserved(
    store: &Store,
    record: &RealtimeFinish,
) -> Result<Finished, InferenceError> {
    let _queued = gate(&store.lock_gates.settlement).await;
    locks::retry_deadlocks!("settlement", finish_once(store, record).await)
}
async fn finish_once(store: &Store, record: &RealtimeFinish) -> Result<Finished, InferenceError> {
    let telemetry = record.telemetry.for_outcome(record.outcome);
    let ms = |v: Option<u64>| v.map(|n| n.min(i64::MAX as u64) as i64);
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    settlement_prefix(store, &mut tx).await?;
    let r = reservation(&mut tx, record.id, scoped(store)).await?;
    let s = session(&mut tx, record.id).await?;
    if r.state != "pending" {
        // Lease expiry already marked it unknown; never overwrite.
        return Err(InferenceError::Storage);
    }
    // The ledger row is written directly: its components are the per-key sum
    // of the settled responses' (validated) components.
    // A forwarded request that never resolved may be running upstream: its
    // window is retained as an unknown response.
    if record.unopened_request {
        let hold = match pinned(&mut tx, &s).await? {
            Some(p) => p.realtime_window(record.window)?,
            None => None,
        };
        sqlx::query("INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,unbounded_cost,completed_at) SELECT $1,coalesce(max(sequence),0)+1,'unknown',$2,$3,clock_timestamp() FROM realtime_responses WHERE execution_id=$1")
            .bind(record.id).bind(hold).bind(hold.is_none())
            .execute(&mut *tx).await.map_err(storage)?;
    }
    // Responses still open at the end never reported usage.
    sqlx::query("UPDATE realtime_responses SET state='unknown',completed_at=clock_timestamp() WHERE execution_id=$1 AND state='pending'")
        .bind(record.id).execute(&mut *tx).await.map_err(storage)?;
    let t: Totals = sqlx::query_as(r#"SELECT count(*) responses,count(*) FILTER(WHERE state='settled') settled,
        coalesce(sum(actual_microusd) FILTER(WHERE state='settled'),0)::bigint actual,
        CASE WHEN bool_or(state='unknown' AND window_hold_microusd IS NULL) THEN NULL ELSE coalesce(sum(greatest(window_hold_microusd,coalesce(floor_microusd,0))) FILTER(WHERE state='unknown'),0) END::bigint retained,
        coalesce(sum(floor_microusd) FILTER(WHERE state='unknown'),0)::bigint known_floor,
        coalesce(bool_or(unbounded_cost),false) unbounded,
        count(*) FILTER(WHERE input_text_tokens IS NOT NULL) measured,
        coalesce(sum(input_text_tokens+input_audio_tokens),0)::bigint input,
        coalesce(sum(output_text_tokens+output_audio_tokens),0)::bigint output,
        (SELECT jsonb_object_agg(k,v::text) FROM (SELECT c.key k,sum((c.value#>>'{}')::numeric) v FROM realtime_responses x,jsonb_each(x.cost_components) c WHERE x.execution_id=$1 AND x.state='settled' GROUP BY c.key) a) components
        FROM realtime_responses WHERE execution_id=$1"#)
        .bind(record.id).fetch_one(&mut *tx).await.map_err(storage)?;
    let all_settled = t.responses == t.settled;
    let actual = all_settled.then_some(t.actual);
    let components = t.components.filter(|_| all_settled && t.settled > 0);
    let (input, output) = if t.measured == t.responses {
        (Some(t.input), Some(t.output))
    } else {
        (None, None)
    };
    let held = if all_settled {
        None
    } else {
        t.retained.and_then(|r| r.checked_add(t.actual))
    };
    let meters = MeterUsage {
        output_images: Some(0),
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some(t.responses as u64),
        ..MeterUsage::default()
    };
    let meter_json = serde_json::to_value(meters).map_err(|_| InferenceError::Storage)?;
    lock_rows(store, &mut tx, &[r.touch()], false).await?;
    let changed = sqlx::query("UPDATE inference_executions SET state=$2,error_code=$3,input_tokens=$4,output_tokens=$5,billing_usage=NULL,elapsed_ms=$6,completed_at=clock_timestamp(),meter_usage=$7,finish_reason=$8,time_to_first_token_ms=$9,generation_ms=$10 WHERE id=$1 AND state='started'")
        .bind(record.id).bind(record.outcome.as_str()).bind(record.error.map(|e| e.code()))
        .bind(input).bind(output).bind(record.elapsed_ms.min(i64::MAX as u64) as i64).bind(&meter_json)
        .bind(telemetry.finish_reason.map(|f| f.as_str())).bind(ms(telemetry.time_to_first_token_ms)).bind(ms(telemetry.generation_ms))
        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    sqlx::query("UPDATE governance_reservations SET state=$2,actual_microusd=$3,input_tokens=$4,output_tokens=$5,billing_usage=NULL,cost_components=$6,held_microusd=CASE WHEN $7 THEN $8 ELSE held_microusd END,unbounded_cost=unbounded_cost OR $9,meter_usage=$10 WHERE execution_id=$1")
        .bind(record.id).bind(if all_settled { "settled" } else { "unknown" }).bind(actual)
        .bind(input).bind(output).bind(&components)
        .bind(!all_settled).bind(held).bind(t.unbounded || (!all_settled && held.is_none()))
        .bind(&meter_json)
        .execute(&mut *tx).await.map_err(storage)?;
    // admitted_at (0030): the ledger is partitioned with its reservation.
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,admitted_at) SELECT $1,$2,$3,$4,$5,$6,NULL,$7,NULL,$8,r.admitted_at FROM governance_reservations r WHERE r.execution_id=$2")
        .bind(Uuid::now_v7()).bind(record.id)
        .bind(if all_settled { "settlement" } else { "unknown" })
        .bind(actual.or(t.actual.checked_add(t.known_floor)))
        .bind(input).bind(output).bind(&components).bind(&meter_json)
        .execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)?;
    Ok((r.provider, r.public_model, all_settled, input, output))
}

/// Lease expiry (`reconcile_expired`): responses still open when a session's
/// process died never reported usage; their windows stay held as unknown.
pub(super) async fn expire_open_responses(tx: &mut Tx<'_>, id: Uuid) -> Result<(), InferenceError> {
    sqlx::query("UPDATE realtime_responses SET state='unknown',completed_at=clock_timestamp() WHERE execution_id=$1 AND state='pending'")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}
