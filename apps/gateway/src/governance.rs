//! Durable per-attempt accounting. Catalog lock precedes the installation row lock.
use crate::{
    billing::{
        self, BillingUsage, CachePricing, CacheRate, CostBreakdown, MeterUsage, MeterVariant,
        v3::{MaxUnits, PriceLines},
    },
    inference::{
        error::{InferenceError, LimitScope},
        repository::{ExecutionFinish, ExecutionStart, Outcome},
        types::{ApiProtocol, ChatRequest, Deployment, EmbeddingRequest, Usage, WorkloadKind},
        workload::{OutputReservation, WorkloadAdmission},
    },
    store::Store,
};
use chrono::{DateTime, DurationRound, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
fn storage(_: sqlx::Error) -> InferenceError {
    InferenceError::Storage
}
async fn lock(tx: &mut Tx<'_>) -> Result<(), InferenceError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE")
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}
#[derive(sqlx::FromRow)]
struct Price {
    id: Uuid,
    /// Null for pricing v3, whose token rates live in `price_lines`.
    input_microusd_per_million: Option<i64>,
    output_microusd_per_million: Option<i64>,
    input_token_limit: i64,
    output_token_limit: i64,
    pricing_version: i16,
    cache_pricing: Option<serde_json::Value>,
    price_lines: Option<serde_json::Value>,
    max_units: Option<serde_json::Value>,
}
const PRICE_COLUMNS: &str = "id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing,price_lines,max_units";
impl Price {
    fn rates(&self) -> Result<CachePricing, InferenceError> {
        serde_json::from_value(self.cache_pricing.clone().ok_or(InferenceError::Storage)?)
            .map_err(|_| InferenceError::Storage)
    }
    fn token_rates(&self) -> Result<(i64, i64), InferenceError> {
        self.input_microusd_per_million
            .zip(self.output_microusd_per_million)
            .ok_or(InferenceError::Storage)
    }
    fn lines(&self) -> Result<(PriceLines, MaxUnits), InferenceError> {
        let lines =
            serde_json::from_value(self.price_lines.clone().ok_or(InferenceError::Storage)?)
                .map_err(|_| InferenceError::Storage)?;
        let max = serde_json::from_value(self.max_units.clone().ok_or(InferenceError::Storage)?)
            .map_err(|_| InferenceError::Storage)?;
        Ok((lines, max))
    }
    /// `ceilings` are request-derived hard unit bounds; they only tighten v3
    /// `max_units` (v1/v2 have no unit meters).
    fn bound(&self, output: u64, ceilings: &MeterUsage) -> Result<Option<i64>, InferenceError> {
        let input =
            u64::try_from(self.input_token_limit).map_err(|_| InferenceError::Configuration)?;
        match self.pricing_version {
            1 => {
                let (i, o) = self.token_rates()?;
                Ok(Some(cost(input, output, i, o)?))
            }
            2 => {
                let (i, o) = self.token_rates()?;
                billing::bound_v2(i, o, &self.rates()?, input, output)
                    .map_err(|_| InferenceError::Configuration)
            }
            3 => {
                let (lines, mut max) = self.lines()?;
                for (meter, ceiling) in billing::v3::Meter::ALL
                    .into_iter()
                    .filter(|m| !m.is_token())
                    .zip(ceilings.counts())
                {
                    // A request ceiling tightens the price's `max_units`. A
                    // request that can exceed it (e.g. measured audio longer
                    // than the priced maximum) is not covered by the price:
                    // the meter becomes unbounded instead of under-held.
                    match (ceiling, max.0.get(&meter).copied()) {
                        (Some(c), Some(m)) if c > m => {
                            max.0.remove(&meter);
                        }
                        (Some(c), _) => {
                            max.0.insert(meter, c);
                        }
                        (None, _) => {}
                    }
                }
                billing::v3::bound(&lines, &max, input, output)
                    .map_err(|_| InferenceError::Configuration)
            }
            _ => Err(InferenceError::Configuration),
        }
    }
}
fn cost(input: u64, output: u64, i: i64, o: i64) -> Result<i64, InferenceError> {
    checked_cost(input, output, i, o).map_err(|_| InferenceError::Storage)
}
fn checked_cost(input: u64, output: u64, i: i64, o: i64) -> Result<i64, billing::BillingError> {
    billing::charge(input, i).and_then(|a| {
        billing::charge(output, o)
            .and_then(|b| a.checked_add(b).ok_or(billing::BillingError::Overflow))
    })
}
/// Budget window of one policy budget. Each scope (installation, type
/// default, workspace override, workspace local, key lineage) may hold at most
/// one budget per period; every applicable budget is enforced over its own
/// current UTC window. Changing budgets never resets or rewrites history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BudgetPeriod {
    /// UTC calendar day.
    Day,
    /// ISO week: Monday 00:00 UTC to the next Monday.
    Week,
    /// UTC calendar month.
    Month,
    /// All time since the scope was created.
    Lifetime,
}
impl BudgetPeriod {
    pub const ALL: [BudgetPeriod; 4] = [
        BudgetPeriod::Day,
        BudgetPeriod::Week,
        BudgetPeriod::Month,
        BudgetPeriod::Lifetime,
    ];
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "lifetime" => Some(Self::Lifetime),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Lifetime => "lifetime",
        }
    }
    /// Half-open UTC window `[start, end)` containing `at`. A lifetime budget
    /// spans every admission (no consumption predates its scope).
    pub fn window(self, at: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        use chrono::{Datelike, NaiveDate, TimeDelta};
        let date = at.date_naive();
        let midnight = |d: NaiveDate| d.and_time(chrono::NaiveTime::MIN).and_utc();
        match self {
            Self::Day => {
                let start = midnight(date);
                (start, start + TimeDelta::days(1))
            }
            Self::Week => {
                let monday =
                    date - TimeDelta::days(i64::from(date.weekday().num_days_from_monday()));
                let start = midnight(monday);
                (start, start + TimeDelta::days(7))
            }
            Self::Month => {
                let first = date.with_day(1).unwrap_or(date);
                let next = if first.month() == 12 {
                    NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
                }
                .unwrap_or(first);
                (midnight(first), midnight(next))
            }
            Self::Lifetime => (
                DateTime::<Utc>::UNIX_EPOCH,
                NaiveDate::from_ymd_opt(9999, 1, 1)
                    .map(midnight)
                    .unwrap_or(DateTime::<Utc>::MAX_UTC),
            ),
        }
    }
}
#[derive(sqlx::FromRow)]
struct Policy {
    workspace_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    requests_per_minute: Option<i64>,
    tokens_per_minute: Option<i64>,
    concurrent_requests: Option<i64>,
}
/// One admission check: scope (workspace, key lineage), rate caps, budget and its window.
type Check = (
    Option<Uuid>,
    Option<Uuid>,
    [Option<i64>; 3],
    Option<i64>,
    DateTime<Utc>,
    DateTime<Utc>,
);
#[derive(sqlx::FromRow)]
struct Budget {
    workspace_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    period: String,
    amount_microusd: i64,
}
// A replacement HEADER, including all-null, replaces the type default. Local/key
// policies compose, never coalesce away a stricter parent. Type limits are per workspace.
const POLICIES: &str = "SELECT NULL::uuid workspace_id,NULL::uuid api_key_id,requests_per_minute,tokens_per_minute,concurrent_requests FROM installation_policy
 UNION ALL SELECT $1::uuid,NULL::uuid,p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests FROM workspace_platform_policy_overrides p WHERE workspace_id=$1
 UNION ALL SELECT $1::uuid,NULL::uuid,p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests FROM workspace_type_policies p JOIN workspaces w ON w.kind=p.kind WHERE w.id=$1 AND NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)
 UNION ALL SELECT $1::uuid,NULL::uuid,requests_per_minute,tokens_per_minute,concurrent_requests FROM workspace_local_policies WHERE workspace_id=$1
 UNION ALL SELECT $1::uuid,$2::uuid,requests_per_minute,tokens_per_minute,concurrent_requests FROM key_policies WHERE workspace_id=$1 AND governance_key_id=$2";
/// Every applicable budget (one per scope and period). Override budgets apply
/// only while the replacement header exists; type budgets only without one.
/// Each budget is checked over its own window; a child can never loosen a
/// parent because the parent's own window check still applies.
pub(crate) const BUDGETS: &str = "SELECT NULL::uuid workspace_id,NULL::uuid api_key_id,period,amount_microusd FROM policy_budgets WHERE layer='installation'
 UNION ALL SELECT $1::uuid,NULL::uuid,b.period,b.amount_microusd FROM policy_budgets b JOIN workspace_platform_policy_overrides o ON o.workspace_id=b.workspace_id WHERE b.layer='override' AND b.workspace_id=$1
 UNION ALL SELECT $1::uuid,NULL::uuid,b.period,b.amount_microusd FROM policy_budgets b JOIN workspaces w ON w.kind=b.kind WHERE b.layer='type' AND w.id=$1 AND NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)
 UNION ALL SELECT $1::uuid,NULL::uuid,period,amount_microusd FROM policy_budgets WHERE layer='local' AND workspace_id=$1
 UNION ALL SELECT $1::uuid,$2::uuid,period,amount_microusd FROM policy_budgets WHERE layer='key' AND workspace_id=$1 AND governance_key_id=$2";
/// Budget consumption of one scope (a workspace, or one key lineage inside it)
/// in `[start, end)`: settled actual plus active pending/unknown holds by
/// admission time, and whether unresolved unbounded/unpriced usage makes the
/// sum a lower bound. Used by policy reports; admission uses the same rules.
pub(crate) async fn budget_consumption(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    lineage: Option<Uuid>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(String, bool), sqlx::Error> {
    sqlx::query_as(r#"SELECT coalesce(sum(CASE WHEN r.state='settled' THEN r.actual_microusd ELSE r.held_microusd END),0)::text,
      count(*) FILTER(WHERE r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))>0
      OR EXISTS(SELECT 1 FROM inference_executions e WHERE e.workspace_id=$1 AND e.started_at>=$3 AND e.started_at<$4 AND ($2::uuid IS NULL OR e.api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2)) AND NOT EXISTS(SELECT 1 FROM governance_reservations x WHERE x.execution_id=e.id))
      FROM governance_reservations r WHERE r.workspace_id=$1 AND ($2::uuid IS NULL OR r.api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2)) AND r.admitted_at>=$3 AND r.admitted_at<$4"#)
        .bind(workspace).bind(lineage).bind(start).bind(end).fetch_one(&mut **tx).await
}
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
    )
    .await
}
pub async fn admit_for_deployment(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
    deployment: &Deployment,
) -> Result<(), InferenceError> {
    admit_checked(
        store,
        record,
        generation(request.max_output_tokens),
        lease_seconds,
        Some(deployment),
    )
    .await
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
    )
    .await
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
    admit_checked(store, record, *admission, lease_seconds, Some(deployment)).await
}
async fn admit_checked(
    store: &Store,
    record: &ExecutionStart,
    workload: WorkloadAdmission,
    lease_seconds: i64,
    expected: Option<&Deployment>,
) -> Result<(), InferenceError> {
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(InferenceError::Configuration);
    }
    let workspace = record.principal.workspace_id;
    let key = record.principal.key_id;
    let mut tx = store.pool.begin().await.map_err(storage)?;
    lock(&mut tx).await?;
    let lineage = crate::auth::revalidate(&mut tx, &record.principal)
        .await
        .map_err(storage)?
        .ok_or(InferenceError::ModelUnavailable)?;
    let current=sqlx::query_as::<_,Deployment>("SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region,m.supported_protocols FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$2 AND m.public_name=$3 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$4) OR EXISTS(SELECT 1 FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$4 AND model_id=m.id)) FOR SHARE OF d,p,m")
        .bind(workspace).bind(record.deployment_id).bind(&record.model).bind(lineage).fetch_optional(&mut *tx).await.map_err(storage)?.ok_or(InferenceError::ModelUnavailable)?;
    if current.provider != record.provider
        || expected.is_some_and(|e| {
            current.id != e.id
                || current.provider != e.provider
                || current.upstream_model != e.upstream_model
                || current.credential_ref != e.credential_ref
                || current.endpoint != e.endpoint
                || current.region != e.region
                || current.supported_protocols != e.supported_protocols
        })
    {
        return Err(InferenceError::Configuration);
    }
    if workload.kind != WorkloadKind::Generation
        && !current
            .supported_protocols
            .iter()
            .any(|p| ApiProtocol::parse(p).is_some_and(|p| p.workload() == workload.kind))
    {
        return Err(InferenceError::Configuration);
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
    let lease = now
        .checked_add_signed(chrono::TimeDelta::seconds(lease_seconds))
        .ok_or(InferenceError::Configuration)?;
    let policies = sqlx::query_as::<_, Policy>(POLICIES)
        .bind(workspace)
        .bind(lineage)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
    let budgets = sqlx::query_as::<_, Budget>(BUDGETS)
        .bind(workspace)
        .bind(lineage)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
    let price=sqlx::query_as::<_,Price>(&format!("SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1")).bind(record.deployment_id).fetch_optional(&mut *tx).await.map_err(storage)?;
    let (tokens, held) = if let Some(p) = &price {
        let output = match workload.output {
            OutputReservation::None => 0,
            OutputReservation::Requested(max) => i64::from(
                max.filter(|n| *n > 0)
                    .ok_or(InferenceError::Configuration)?,
            ),
            OutputReservation::PriceCeiling => p.output_token_limit,
        };
        // A zero input ceiling is valid only for v3 prices whose input-family
        // token meters are all not applicable (validated again here).
        let zero_input_ok = p.input_token_limit == 0
            && p.pricing_version == 3
            && p.lines()?.0.input_tokens_inapplicable();
        if (p.input_token_limit < 1 && !zero_input_ok) || output > p.output_token_limit {
            return Err(InferenceError::Configuration);
        }
        (
            Some(
                p.input_token_limit
                    .checked_add(output)
                    .ok_or(InferenceError::Configuration)?,
            ),
            p.bound(output as u64, &workload.unit_ceilings)?,
        )
    } else {
        (None, None)
    };
    // Pricing v3 reports an unbounded (unknown-rate or uncapped) meter under a
    // budget as a budget denial at that scope. v1/v2 keep their legacy
    // configuration error.
    let v3_unbounded = price.as_ref().is_some_and(|p| p.pricing_version == 3) && held.is_none();
    if policies
        .iter()
        .any(|p| p.tokens_per_minute.is_some() && tokens.is_none())
        || (!budgets.is_empty() && held.is_none() && !v3_unbounded)
    {
        return Err(InferenceError::Configuration);
    }
    // Rate layers and every (scope, period) budget are separate checks; a rate
    // check uses an empty budget window.
    let minute = now
        .duration_trunc(chrono::TimeDelta::minutes(1))
        .map_err(|_| InferenceError::Configuration)?;
    let mut checks: Vec<Check> = policies
        .into_iter()
        .map(|p| {
            (
                p.workspace_id,
                p.api_key_id,
                [
                    p.requests_per_minute,
                    p.tokens_per_minute,
                    p.concurrent_requests,
                ],
                None,
                minute,
                minute,
            )
        })
        .collect();
    for b in budgets {
        let period = BudgetPeriod::parse(&b.period).ok_or(InferenceError::Storage)?;
        // This budget's own UTC window: [start, end) by admission time.
        let (start, end) = period.window(now);
        checks.push((
            b.workspace_id,
            b.api_key_id,
            [None; 3],
            Some(b.amount_microusd),
            start,
            end,
        ));
    }
    // Evaluate every applicable layer, then report the most actionable denial:
    // budget/accounting (narrowest scope first) before retryable rate limits.
    let mut denial: Option<(u8, InferenceError)> = None;
    for (
        workspace_id,
        api_key_id,
        [requests_per_minute, tokens_per_minute, concurrent_requests],
        budget,
        window_start,
        window_end,
    ) in checks
    {
        let (rate_ok, unresolved_ok, budget_ok):(bool,bool,bool)=sqlx::query_as(r#"WITH accounting AS (
          SELECT workspace_id,api_key_id,minute_start,admitted_at budget_at,state,lease_expires_at,reserved_tokens,input_tokens,output_tokens,billing_usage,held_microusd,actual_microusd,unbounded_cost FROM governance_reservations
          UNION ALL SELECT e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),e.started_at,'unknown',NULL::timestamptz,NULL::bigint,e.input_tokens,e.output_tokens,e.billing_usage,NULL::bigint,NULL::bigint,true FROM inference_executions e WHERE e.started_at>=least($10::timestamptz,date_trunc('minute',$3::timestamptz,'UTC')) AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)
          ) SELECT
          ($4::bigint IS NULL OR count(*) FILTER(WHERE minute_start=date_trunc('minute',$3::timestamptz,'UTC'))::numeric+1<=$4)
          AND ($5::bigint IS NULL OR (count(*) FILTER(WHERE minute_start=date_trunc('minute',$3::timestamptz,'UTC') AND reserved_tokens IS NULL)=0 AND coalesce(sum(greatest(coalesce(reserved_tokens,0)::numeric,coalesce(input_tokens,0)::numeric+coalesce(output_tokens,0)::numeric,coalesce((billing_usage->>'total_input_tokens')::numeric,0)+coalesce(output_tokens,0)::numeric,coalesce((billing_usage->>'uncached_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_read_input_tokens')::numeric,0)+greatest(coalesce((billing_usage->>'cache_write_input_tokens')::numeric,0),coalesce((billing_usage->>'cache_write_default_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_write_5m_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_write_1h_input_tokens')::numeric,0))+coalesce(output_tokens,0)::numeric)) FILTER(WHERE minute_start=date_trunc('minute',$3::timestamptz,'UTC')),0)+$8::bigint<=$5))
          AND ($6::bigint IS NULL OR count(*) FILTER(WHERE state='pending' AND lease_expires_at>$3)::numeric+1<=$6),
          ($7::bigint IS NULL OR count(*) FILTER(WHERE budget_at>=$10 AND budget_at<$11 AND state<>'settled' AND (unbounded_cost OR held_microusd IS NULL))=0),
          ($7::bigint IS NULL OR coalesce(sum(CASE WHEN state='settled' THEN actual_microusd ELSE held_microusd END) FILTER(WHERE budget_at>=$10 AND budget_at<$11),0)+$9::bigint<=$7)
          FROM accounting WHERE ($1::uuid IS NULL OR workspace_id=$1) AND ($2::uuid IS NULL OR api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2)) AND (minute_start=date_trunc('minute',$3::timestamptz,'UTC') OR (budget_at>=$10 AND budget_at<$11) OR (state='pending' AND lease_expires_at>$3))"#)
            .bind(workspace_id).bind(api_key_id).bind(now).bind(requests_per_minute).bind(tokens_per_minute).bind(concurrent_requests).bind(budget).bind(tokens).bind(if v3_unbounded { Some(0) } else { held }).bind(window_start).bind(window_end).fetch_one(&mut *tx).await.map_err(storage)?;
        let budget_ok = budget_ok && !(v3_unbounded && budget.is_some());
        let scope = match (workspace_id, api_key_id) {
            (None, _) => LimitScope::Installation,
            (Some(_), None) => LimitScope::Workspace,
            (Some(_), Some(_)) => LimitScope::ApiKey,
        };
        let rank = |scope| match scope {
            LimitScope::ApiKey => 3,
            LimitScope::Workspace => 2,
            LimitScope::Installation => 1,
        };
        // A reservation that alone exceeds a tokens-per-minute limit can never
        // be admitted, however idle the minute is: report it honestly (scope
        // kind only, never amounts) instead of a transient rate limit.
        let reservation_too_large = tokens_per_minute
            .zip(tokens)
            .is_some_and(|(limit, reserved)| reserved > limit);
        let found = if !unresolved_ok {
            // Installation-wide holds can belong to other workspaces; never
            // reveal whether another scope has unresolved usage.
            Some((
                rank(scope),
                match scope {
                    LimitScope::Installation => InferenceError::BudgetExceeded(scope),
                    _ => InferenceError::UnresolvedUsage(scope),
                },
            ))
        } else if !budget_ok {
            Some((rank(scope), InferenceError::BudgetExceeded(scope)))
        } else if reservation_too_large {
            Some((
                rank(scope),
                InferenceError::TokenReservationExceedsLimit(scope),
            ))
        } else if !rate_ok {
            Some((0, InferenceError::Busy))
        } else {
            None
        };
        if let Some(found) = found
            && denial.is_none_or(|d| found.0 > d.0)
        {
            denial = Some(found);
        }
    }
    if let Some((_, error)) = denial {
        return Err(error);
    }
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,root_request_id,attempt_number,workload_kind,cost_center_id,cost_center_name,cost_center_code,upstream_model,client_session_id,client_app) SELECT $1,w.id,$3,$4,$5,$6,$7,'started',$8,$9,$10,$11,w.cost_center_id,c.name,c.code,$12,$13,$14 FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$2")
        .bind(record.id).bind(workspace).bind(key).bind(record.deployment_id).bind(&record.model).bind(&record.provider).bind(record.streamed).bind(now).bind(record.root_request_id).bind(record.attempt_number).bind(workload.kind.as_str()).bind(&record.upstream_model).bind(&record.client.session_id).bind(&record.client.app).execute(&mut *tx).await.map_err(storage)?;
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,unbounded_cost) VALUES($1,$2,$3,$4,$5,$6,date_trunc('minute',$6::timestamptz,'UTC'),date_trunc('month',$6::timestamptz,'UTC'),$7,'pending',$8,$9,$10)")
        .bind(record.id).bind(workspace).bind(key).bind(record.deployment_id).bind(price.as_ref().map(|p|p.id)).bind(now).bind(lease).bind(tokens).bind(held).bind(held.is_none()).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        record.id,
        "hold",
        held,
        Usage::default(),
        None,
        None,
    )
    .await?;
    tx.commit().await.map_err(storage)
}
#[derive(sqlx::FromRow)]
struct Reservation {
    workspace_id: Uuid,
    deployment_id: Uuid,
    price_id: Option<Uuid>,
    state: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    billing_usage: Option<serde_json::Value>,
    workload_kind: String,
    reserved_tokens: Option<i64>,
    meter_usage: Option<serde_json::Value>,
    output_image_variant: Option<String>,
    provider_cost_microusd: Option<i64>,
}
async fn reservation(tx: &mut Tx<'_>, id: Uuid) -> Result<Reservation, InferenceError> {
    sqlx::query_as("SELECT r.workspace_id,r.deployment_id,r.price_id,r.state,r.input_tokens,r.output_tokens,r.billing_usage,e.workload_kind,r.reserved_tokens,r.meter_usage,r.output_image_variant,r.provider_cost_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=$1").bind(id).fetch_one(&mut **tx).await.map_err(storage)
}
/// Meter, variant and provider-cost evidence as stored columns.
struct MeterEvidence {
    meters: Option<serde_json::Value>,
    variant: Option<String>,
    provider_cost: Option<i64>,
}
fn meter_evidence(usage: Usage) -> Result<MeterEvidence, InferenceError> {
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
fn usage_values(usage: Usage) -> Result<(Option<i64>, Option<i64>), InferenceError> {
    meter_evidence(usage)?;
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
fn billing_json(usage: Usage) -> Result<Option<serde_json::Value>, InferenceError> {
    usage
        .billing
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| InferenceError::Storage)
}
fn workload_usage(r: &Reservation, mut usage: Usage) -> Result<Usage, InferenceError> {
    // Input-only workloads: output zero is semantic non-applicability.
    if matches!(r.workload_kind.as_str(), "embeddings" | "rerank") {
        if usage.output_tokens.is_some_and(|n| n != 0) {
            return Err(InferenceError::Storage);
        }
        usage.output_tokens = Some(0);
    }
    usage_values(usage)?;
    Ok(usage)
}
struct Valuation {
    actual: Option<i64>,
    floor: Option<i64>,
    components: Option<CostBreakdown>,
    violated: bool,
}
/// A finite hold must cover every category still possible under the inclusive
/// ceiling. Unknown-rate categories are possible whenever capacity remains;
/// not-applicable categories only when observed evidence forces usage into them. Residuals constrain possibilities; they are not observed allocations
/// and must never be fed into settlement or the known monetary floor.
fn cache_bound_violated(
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
async fn pinned_value(
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
    let p = sqlx::query_as::<_, Price>(&format!(
        "SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE id=$1 AND deployment_id=$2"
    ))
    .bind(id)
    .bind(r.deployment_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(storage)?;
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
    let output_bound = r
        .reserved_tokens
        .and_then(|n| n.checked_sub(p.input_token_limit));
    value.violated |= observed_input > p.input_token_limit as u64
        || output_bound.is_some_and(|limit| usage.output_tokens.is_some_and(|n| n > limit as u64));
    Ok(value)
}
async fn ledger(
    tx: &mut Tx<'_>,
    id: Uuid,
    kind: &str,
    amount: Option<i64>,
    usage: Usage,
    components: Option<CostBreakdown>,
    evidence: Option<&str>,
) -> Result<(), InferenceError> {
    let (input, output) = usage_values(usage)?;
    let m = meter_evidence(usage)?;
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,evidence,meter_usage,output_image_variant,provider_cost_microusd) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
        .bind(Uuid::new_v4()).bind(id).bind(kind).bind(amount).bind(input).bind(output).bind(billing_json(usage)?).bind(components.map(|c|c.to_value())).bind(evidence).bind(m.meters).bind(m.variant).bind(m.provider_cost).execute(&mut **tx).await.map_err(storage)?;
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
    let p = sqlx::query_as::<_, Price>(&format!(
        "SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE id=$1 AND deployment_id=$2"
    ))
    .bind(id)
    .bind(r.deployment_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(storage)?;
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
        || record.error != Some(InferenceError::UpstreamRejected)
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
    let telemetry = telemetry.for_outcome(record.outcome);
    let ms = |v: Option<u64>| v.map(|n| n.min(i64::MAX as u64) as i64);
    let reasoning = record
        .usage
        .reasoning_tokens
        .filter(|n| *n <= i64::MAX as u64)
        .map(|n| n as i64);
    let mut tx = store.pool.begin().await.map_err(storage)?;
    lock(&mut tx).await?;
    let r = reservation(&mut tx, record.id).await?;
    let mut usage = workload_usage(&r, record.usage)?;
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
        return if same {
            Ok(())
        } else {
            Err(InferenceError::Storage)
        };
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
    let changed=sqlx::query("UPDATE inference_executions SET state=$2,error_code=$3,input_tokens=$4,output_tokens=$5,billing_usage=$6,elapsed_ms=$7,completed_at=clock_timestamp(),meter_usage=$8,output_image_variant=$9,provider_cost_microusd=$10,finish_reason=$11,time_to_first_token_ms=$12,generation_ms=$13,reasoning_tokens=$14 WHERE id=$1 AND state='started'")
        .bind(record.id).bind(record.outcome.as_str()).bind(record.error.map(|e|e.code())).bind(input).bind(output).bind(&billing).bind(record.elapsed_ms.min(i64::MAX as u64)as i64).bind(&m.meters).bind(&m.variant).bind(m.provider_cost)
        .bind(telemetry.finish_reason.map(|f|f.as_str())).bind(ms(telemetry.time_to_first_token_ms)).bind(ms(telemetry.generation_ms)).bind(reasoning).execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    sqlx::query("UPDATE governance_reservations SET state=$2,actual_microusd=$3,input_tokens=$4,output_tokens=$5,billing_usage=$6,cost_components=$7,held_microusd=CASE WHEN $8::bigint IS NULL THEN held_microusd ELSE greatest(held_microusd,$8) END,unbounded_cost=unbounded_cost OR $9,meter_usage=$10,output_image_variant=$11,provider_cost_microusd=$12 WHERE execution_id=$1")
        .bind(record.id).bind(if actual.is_some(){"settled"}else{"unknown"}).bind(actual).bind(input).bind(output).bind(billing).bind(components.map(|c|c.to_value())).bind(value.floor.filter(|_|actual.is_none())).bind(value.violated).bind(m.meters).bind(m.variant).bind(m.provider_cost).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        record.id,
        if actual.is_some() {
            "settlement"
        } else {
            "unknown"
        },
        actual.or(value.floor),
        usage,
        components,
        None,
    )
    .await?;
    tx.commit().await.map_err(storage)
}
pub async fn reconcile_expired(store: &Store, limit: i64) -> Result<u64, InferenceError> {
    if !(1..=10_000).contains(&limit) {
        return Err(InferenceError::InvalidRequest);
    }
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT execution_id FROM governance_reservations WHERE state='pending' AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at,execution_id LIMIT $1").bind(limit).fetch_all(&store.pool).await.map_err(storage)?;
    let mut count = 0;
    for id in ids {
        let mut tx = store.pool.begin().await.map_err(storage)?;
        lock(&mut tx).await?;
        let changed=sqlx::query("UPDATE governance_reservations SET state='unknown' WHERE execution_id=$1 AND state='pending' AND lease_expires_at<=clock_timestamp()").bind(id).execute(&mut *tx).await.map_err(storage)?.rows_affected();
        if changed == 1 {
            let changed=sqlx::query("UPDATE inference_executions SET state='cancelled',error_code='lease_expired',finish_reason='cancelled',completed_at=clock_timestamp() WHERE id=$1 AND state='started'").bind(id).execute(&mut *tx).await.map_err(storage)?.rows_affected();
            if changed != 1 {
                return Err(InferenceError::Storage);
            }
            ledger(&mut tx, id, "unknown", None, Usage::default(), None, None).await?;
            count += 1;
        }
        tx.commit().await.map_err(storage)?;
    }
    Ok(count)
}
/// Authorization remains live after waiting. Personal details never become platform-public.
pub async fn resolve_usage(
    store: &Store,
    workspace: Uuid,
    execution: Uuid,
    usage: Usage,
    evidence: &str,
    actor: Uuid,
) -> Result<(), InferenceError> {
    if evidence.trim().is_empty()
        || evidence.chars().count() > 200
        || evidence.chars().any(char::is_control)
    {
        return Err(InferenceError::InvalidRequest);
    }
    let mut tx = store.pool.begin().await.map_err(storage)?;
    lock(&mut tx).await?;
    let r = reservation(&mut tx, execution).await?;
    if r.workspace_id != workspace {
        return Err(InferenceError::InvalidRequest);
    }
    let authorized:Option<Uuid>=sqlx::query_scalar("SELECT u.id FROM users u JOIN effective_platform_roles p ON p.user_id=u.id AND p.role='admin' JOIN workspaces w ON w.id=$2 AND w.disabled_at IS NULL WHERE u.id=$1 AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND (w.kind IN('team','project') OR w.owner_user_id=u.id) FOR SHARE OF u,w")
        .bind(actor).bind(workspace).fetch_optional(&mut *tx).await.map_err(storage)?;
    if authorized.is_none() {
        return Err(InferenceError::InvalidRequest);
    }
    let usage = workload_usage(&r, usage)?;
    let (input, output) = usage_values(usage)?;
    let billing = billing_json(usage)?;
    let m = meter_evidence(usage)?;
    if input.is_none() || output.is_none() {
        return Err(InferenceError::InvalidRequest);
    }
    if r.state == "settled" {
        let same:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation' AND input_tokens IS NOT DISTINCT FROM $2::bigint AND output_tokens IS NOT DISTINCT FROM $3::bigint AND billing_usage IS NOT DISTINCT FROM $4::jsonb AND evidence=$5 AND meter_usage IS NOT DISTINCT FROM $6::jsonb AND output_image_variant IS NOT DISTINCT FROM $7::text AND provider_cost_microusd IS NOT DISTINCT FROM $8::bigint)").bind(execution).bind(input).bind(output).bind(billing).bind(evidence).bind(m.meters).bind(m.variant).bind(m.provider_cost).fetch_one(&mut *tx).await.map_err(storage)?;
        return if same {
            Ok(())
        } else {
            Err(InferenceError::InvalidRequest)
        };
    }
    let terminal: bool =
        sqlx::query_scalar("SELECT state<>'started' FROM inference_executions WHERE id=$1")
            .bind(execution)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
    let preserved_billing = if let Some(old) = &r.billing_usage {
        let old: BillingUsage =
            serde_json::from_value(old.clone()).map_err(|_| InferenceError::Storage)?;
        usage.billing.is_some_and(|new| new.preserves(&old))
    } else {
        true
    };
    // Meter, variant and provider-cost evidence may be added, never erased or changed.
    let preserved_meters = match &r.meter_usage {
        Some(old) => {
            let old: MeterUsage =
                serde_json::from_value(old.clone()).map_err(|_| InferenceError::Storage)?;
            usage.meters.is_some_and(|new| new.preserves(&old))
        }
        None => true,
    } && r
        .output_image_variant
        .as_ref()
        .is_none_or(|old| m.variant.as_ref() == Some(old))
        && r.provider_cost_microusd
            .is_none_or(|old| m.provider_cost == Some(old));
    if r.state != "unknown"
        || !preserved_meters
        || !terminal
        || r.input_tokens
            .is_some_and(|old| input.is_none_or(|new| new < old))
        || r.output_tokens
            .is_some_and(|old| output.is_none_or(|new| new < old))
        || !preserved_billing
    {
        return Err(InferenceError::InvalidRequest);
    }
    let value = pinned_value(&mut tx, &r, usage).await?;
    let actual = value.actual.ok_or(InferenceError::Configuration)?;
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=$2,input_tokens=$3,output_tokens=$4,billing_usage=$5,cost_components=$6,meter_usage=$7,output_image_variant=$8,provider_cost_microusd=$9 WHERE execution_id=$1").bind(execution).bind(actual).bind(input).bind(output).bind(&billing).bind(value.components.map(|c|c.to_value())).bind(&m.meters).bind(&m.variant).bind(m.provider_cost).execute(&mut *tx).await.map_err(storage)?;
    sqlx::query("UPDATE inference_executions SET input_tokens=$2,output_tokens=$3,billing_usage=$4,meter_usage=$5,output_image_variant=$6,provider_cost_microusd=$7 WHERE id=$1").bind(execution).bind(input).bind(output).bind(billing).bind(m.meters).bind(m.variant).bind(m.provider_cost).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        execution,
        "reconciliation",
        Some(actual),
        usage,
        value.components,
        Some(evidence),
    )
    .await?;
    sqlx::query("INSERT INTO audit_events(id,workspace_id,actor_user_id,action,resource_type,resource_id,metadata) VALUES($1,$2,$3,'usage.reconciled','execution',$4,'{}')").bind(Uuid::new_v4()).bind(workspace).bind(actor).bind(execution).execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)
}
#[cfg(all(test, feature = "integration-tests"))]
mod audio_tests;
/// Test helper: replace one scope's whole budget set with `amount` for `period` (or none).
#[cfg(test)]
pub(crate) async fn set_test_budget(
    pool: &sqlx::PgPool,
    layer: &str,
    kind: Option<&str>,
    workspace: Option<Uuid>,
    key: Option<Uuid>,
    period: &str,
    amount: Option<i64>,
) {
    sqlx::query("DELETE FROM policy_budgets WHERE layer=$1 AND kind IS NOT DISTINCT FROM $2 AND workspace_id IS NOT DISTINCT FROM $3 AND governance_key_id IS NOT DISTINCT FROM $4").bind(layer).bind(kind).bind(workspace).bind(key).execute(pool).await.unwrap();
    if let Some(amount) = amount {
        sqlx::query("INSERT INTO policy_budgets(layer,kind,workspace_id,governance_key_id,period,amount_microusd) VALUES($1,$2,$3,$4,$5,$6)").bind(layer).bind(kind).bind(workspace).bind(key).bind(period).bind(amount).execute(pool).await.unwrap();
    }
}
#[cfg(all(test, feature = "integration-tests"))]
mod image_tests;
#[cfg(test)]
mod period_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod safety_tests;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(all(test, feature = "integration-tests"))]
mod v3_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod workload_tests;
