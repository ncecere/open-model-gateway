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
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
fn storage(_: sqlx::Error) -> InferenceError {
    InferenceError::Storage
}
/// Bounded in-process queues in front of the installation lock, one for
/// admissions and one for settlements/reconciliation. Work under that lock is
/// serialized anyway, so letting every concurrent request hold a pooled
/// connection while it waits adds no throughput: it starves authentication,
/// routing and settlement of connections (the load test saw 503s and
/// unsettled streams at 200 concurrent requests on a 10-connection pool).
/// Each queue lets at most a quarter of the pool wait on the database lock;
/// other callers wait here without a connection. Settlements never queue
/// behind admissions in-process, so holds are released promptly.
pub struct LockGates {
    admission: tokio::sync::Semaphore,
    settlement: tokio::sync::Semaphore,
}
impl LockGates {
    pub(crate) fn for_pool(max_connections: u32) -> Self {
        let permits = (max_connections as usize / 4).max(1);
        Self {
            admission: tokio::sync::Semaphore::new(permits),
            settlement: tokio::sync::Semaphore::new(permits),
        }
    }
}
async fn gate(semaphore: &tokio::sync::Semaphore) -> tokio::sync::SemaphorePermit<'_> {
    // Never closed; an error would mean a bug, and failing open is safe here
    // because the database lock still serializes the work.
    semaphore
        .acquire()
        .await
        .expect("installation lock gate is never closed")
}
async fn lock(tx: &mut Tx<'_>) -> Result<(), InferenceError> {
    catalog_lock(tx).await?;
    installation_lock(tx).await
}
/// The shared catalog lock: catalog writers (deployments, providers, models,
/// prices) hold it exclusively, so it fences catalog configuration.
async fn catalog_lock(tx: &mut Tx<'_>) -> Result<(), InferenceError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}
/// The installation row lock, taken after [`catalog_lock`]. Admission and
/// settlement still serialize on it with authorization, membership and policy
/// changes (management holds it too); it no longer guards any limit data:
/// there are no installation-wide limits (0026) and admission reads only the
/// workspace's and key lineage's own policies, budget totals and counters.
/// Scale plan P3 replaces it in admission with scoped authority locks.
async fn installation_lock(tx: &mut Tx<'_>) -> Result<(), InferenceError> {
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
    /// Batch price list (0021, v3 only): applies to native batches.
    batch_price_lines: Option<serde_json::Value>,
}
const PRICE_COLUMNS: &str = "id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing,price_lines,max_units,batch_price_lines";
impl Price {
    /// Whether this version publishes a batch price list (`batch::PriceTier`).
    fn has_batch_lines(&self) -> bool {
        self.pricing_version == 3 && self.batch_price_lines.is_some()
    }
    /// The price as valued under a reservation's pinned tier (`price_tier`,
    /// 0021): the batch tier swaps in the batch price list.
    fn tiered(mut self, tier: &str) -> Result<Price, InferenceError> {
        match tier {
            "standard" => Ok(self),
            "batch" if self.has_batch_lines() => {
                self.price_lines = self.batch_price_lines.take();
                Ok(self)
            }
            _ => Err(InferenceError::Storage),
        }
    }
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
        self.bound_scaled(1, output, ceilings, false)
    }
    /// [`Price::bound`] for `requests` requests sharing one attempt (an async
    /// batch: input ceiling `requests * input_token_limit`) and, for the
    /// video workload, the `output_video_seconds_ms` meter. Every other
    /// workload cannot produce video, so prices without a video line still
    /// bound them.
    fn bound_scaled(
        &self,
        requests: u64,
        output: u64,
        ceilings: &MeterUsage,
        video: bool,
    ) -> Result<Option<i64>, InferenceError> {
        let input = u64::try_from(self.input_token_limit)
            .ok()
            .and_then(|n| n.checked_mul(requests))
            .ok_or(InferenceError::Configuration)?;
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
                // `MeterUsage::counts` order: non-token `Meter::ALL`, then video.
                for (meter, ceiling) in billing::v3::Meter::ALL
                    .into_iter()
                    .filter(|m| !m.is_token())
                    .chain(billing::v3::Meter::VIDEO)
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
                if video {
                    billing::v3::bound_video(&lines, &max, input, output)
                } else {
                    billing::v3::bound(&lines, &max, input, output)
                }
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
/// Budget window of one policy budget. Each scope (type default, workspace
/// override, workspace local, key lineage) may hold at most
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
/// One applicable policy layer of the admitting workspace: the workspace
/// layers (type default or platform override, then local) or, with
/// `api_key_id`, the key lineage layer. There is no installation layer
/// (removed in 0026): admission reads no installation-wide limit data.
#[derive(sqlx::FromRow)]
struct Policy {
    api_key_id: Option<Uuid>,
    requests_per_minute: Option<i64>,
    tokens_per_minute: Option<i64>,
    concurrent_requests: Option<i64>,
    /// "Jobs at once" (0018): active async jobs (video + batch).
    concurrent_jobs: Option<i64>,
}
/// One applicable budget of the admitting workspace (`api_key_id`: the key
/// lineage layer).
#[derive(sqlx::FromRow)]
struct Budget {
    api_key_id: Option<Uuid>,
    period: String,
    amount_microusd: i64,
}
// A replacement HEADER, including all-null, replaces the type default. Local/key
// policies compose, never coalesce away a stricter parent. Type limits are per workspace.
const POLICIES: &str = "SELECT $1::uuid workspace_id,NULL::uuid api_key_id,p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests,p.concurrent_jobs FROM workspace_platform_policy_overrides p WHERE workspace_id=$1
 UNION ALL SELECT $1::uuid,NULL::uuid,p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests,p.concurrent_jobs FROM workspace_type_policies p JOIN workspaces w ON w.kind=p.kind WHERE w.id=$1 AND NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)
 UNION ALL SELECT $1::uuid,NULL::uuid,requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs FROM workspace_local_policies WHERE workspace_id=$1
 UNION ALL SELECT $1::uuid,$2::uuid,requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs FROM key_policies WHERE workspace_id=$1 AND governance_key_id=$2";
// Rate/concurrency/job accounting reads the maintained counters of
// `rates` (migration 0024): the current UTC minute and live leases only,
// O(scopes), exactly the former per-layer scan (kept as `rates::SCAN`):
// - Job reservations never count toward requests/tokens per minute.
// - A job holds a "requests at once" slot only while its submission runs
//   (no `async_jobs` row yet); afterwards it holds a job slot until the job
//   is terminal, cancel was requested, or the lease expired.
// - Gateway-run batch lines (`batch_job_id`, 0021) count toward nothing:
//   their batch holds the job slot.
/// Every applicable budget (one per scope and period). Override budgets apply
/// only while the replacement header exists; type budgets only without one.
/// Each budget is checked over its own window; a child can never loosen a
/// parent because the parent's own window check still applies.
pub(crate) const BUDGETS: &str = "SELECT $1::uuid workspace_id,NULL::uuid api_key_id,b.period,b.amount_microusd FROM policy_budgets b JOIN workspace_platform_policy_overrides o ON o.workspace_id=b.workspace_id WHERE b.layer='override' AND b.workspace_id=$1
 UNION ALL SELECT $1::uuid,NULL::uuid,b.period,b.amount_microusd FROM policy_budgets b JOIN workspaces w ON w.kind=b.kind WHERE b.layer='type' AND w.id=$1 AND NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)
 UNION ALL SELECT $1::uuid,NULL::uuid,period,amount_microusd FROM policy_budgets WHERE layer='local' AND workspace_id=$1
 UNION ALL SELECT $1::uuid,$2::uuid,period,amount_microusd FROM policy_budgets WHERE layer='key' AND workspace_id=$1 AND governance_key_id=$2";
/// Budget consumption of one scope (a workspace, or one key lineage inside it)
/// in the current window of a period: settled actual plus active
/// pending/unknown holds by admission time, and whether unresolved
/// unbounded/unpriced usage makes the sum a lower bound. Read from the
/// maintained totals (migration 0015) that admission also uses.
pub(crate) use totals::budget_consumption;
/// Batch admission, line admission and closing (`crate::jobs::batch`, 0021).
pub mod batch;
/// Lease extension and bound preview for async jobs (`crate::jobs`).
pub mod jobs;
/// Maintained per-scope, per-minute rate and in-flight counters.
pub mod rates;
/// Maintained per-scope, per-period budget totals and their consistency check.
pub mod totals;
/// Meters a pricing-v3 price of a `kind` route must state explicitly (priced,
/// `not_applicable` or `unknown`) at publication: every meter that kind's
/// admission bound treats as possibly used. Admission bounds every meter of
/// [`billing::v3::Meter::ALL`] except those a workload's request ceilings fix
/// at zero (images, speech to text, text to speech and video jobs fix the unit
/// meters they cannot produce; a realtime window fixes every non-token meter
/// but `requests`), plus the realtime audio-token and video meters. A meter left
/// out would make every budgeted admission unbounded (`price_unbounded`).
pub fn price_meters(kind: WorkloadKind) -> Vec<billing::v3::Meter> {
    use billing::v3::Meter;
    let tokens = Meter::ALL.into_iter().filter(|m| m.is_token());
    match kind {
        WorkloadKind::Images => tokens
            .chain([Meter::OutputImages, Meter::Requests])
            .collect(),
        WorkloadKind::AudioTranscriptions => tokens
            .chain([Meter::InputAudioSecondsMs, Meter::Requests])
            .collect(),
        WorkloadKind::AudioSpeech => tokens
            .chain([
                Meter::InputCharacters,
                Meter::OutputAudioSecondsMs,
                Meter::Requests,
            ])
            .collect(),
        WorkloadKind::Realtime => tokens
            .chain(Meter::AUDIO_TOKENS)
            .chain([Meter::Requests])
            .collect(),
        WorkloadKind::Videos => tokens
            .chain([Meter::Requests])
            .chain(Meter::VIDEO)
            .collect(),
        WorkloadKind::Generation
        | WorkloadKind::Embeddings
        | WorkloadKind::Rerank
        | WorkloadKind::Systemone
        | WorkloadKind::Batches => Meter::ALL.to_vec(),
    }
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
    let mut timer = crate::metrics::PhaseTimer::start();
    let result =
        admit_unobserved(store, record, workload, lease_seconds, expected, &mut timer).await;
    crate::metrics::observe_admission(&result);
    crate::metrics::METRICS
        .observe_admission_phases(timer, crate::metrics::admission_outcome(&result));
    result
}
async fn admit_unobserved(
    store: &Store,
    record: &ExecutionStart,
    workload: WorkloadAdmission,
    lease_seconds: i64,
    expected: Option<&Deployment>,
    timer: &mut crate::metrics::PhaseTimer,
) -> Result<(), InferenceError> {
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(InferenceError::Configuration);
    }
    let workspace = record.principal.workspace_id;
    let key = record.principal.key_id;
    let _queued = gate(&store.lock_gates.admission).await;
    timer.phase("queue");
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    timer.phase("connect");
    // Catalog configuration (deployments, providers, models and their
    // append-only prices) changes only under the exclusive catalog lock, so
    // once it is held shared the latest price and the reservation bounds are
    // fixed for this transaction: resolve them before the installation lock.
    // Authorization, entitlement and policies change under the installation
    // lock and stay below it. Bound errors are reported after the checks
    // below, in the former order.
    catalog_lock(&mut tx).await?;
    let price=sqlx::query_as::<_,Price>(&format!("SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1")).bind(record.deployment_id).fetch_optional(&mut *tx).await.map_err(storage)?;
    let bounds = reservation_bounds(price.as_ref(), &workload);
    timer.phase("price");
    installation_lock(&mut tx).await?;
    timer.phase("locks");
    let lineage = crate::auth::revalidate_admission(&mut tx, &record.principal)
        .await
        .map_err(storage)?
        .ok_or(InferenceError::ModelUnavailable)?;
    // The deployment check also reads the admission clock (one round trip).
    let row=sqlx::query("SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region,m.supported_protocols,clock_timestamp() AS admission_now FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$2 AND m.public_name=$3 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$4) OR EXISTS(SELECT 1 FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$4 AND model_id=m.id)) FOR SHARE OF d,p,m")
        .bind(workspace).bind(record.deployment_id).bind(&record.model).bind(lineage).fetch_optional(&mut *tx).await.map_err(storage)?.ok_or(InferenceError::ModelUnavailable)?;
    let current = <Deployment as sqlx::FromRow<_>>::from_row(&row).map_err(storage)?;
    let database_now: DateTime<Utc> = sqlx::Row::try_get(&row, "admission_now").map_err(storage)?;
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
    let now = store.admission_override().unwrap_or(database_now);
    let lease = now
        .checked_add_signed(chrono::TimeDelta::seconds(lease_seconds))
        .ok_or(InferenceError::Configuration)?;
    let (tokens, held) = bounds?;
    // A pricing-v3 price that cannot bound this attempt (a possibly-used meter
    // without a line, an explicit unknown, or no `max_units`) is refused under
    // any budget with `price_unbounded`. v1/v2 keep their legacy
    // configuration error.
    let v3_unbounded = price.as_ref().is_some_and(|p| p.pricing_version == 3) && held.is_none();
    // Async jobs (video, batch) are exempt from requests/tokens-per-minute
    // limits and are counted by "jobs at once"; budgets apply in full.
    let job = matches!(workload.kind, WorkloadKind::Videos | WorkloadKind::Batches);
    timer.phase("read");
    enforce_limits(
        &mut tx,
        workspace,
        lineage,
        now,
        if job {
            LimitMode::Job
        } else {
            LimitMode::Interactive
        },
        tokens,
        held,
        v3_unbounded,
    )
    .await?;
    timer.phase("limits");
    // An async batch records its request count (0016) for settlement checks.
    let request_count = match workload.output {
        OutputReservation::Batch { requests, .. } => {
            Some(i32::try_from(requests).map_err(|_| InferenceError::Configuration)?)
        }
        _ => None,
    };
    // Execution, reservation and ledger hold in one statement (the totals and
    // counter triggers fire at its end); the hold row is what `ledger` writes
    // for a hold without usage.
    let written = sqlx::query("WITH e AS (INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,root_request_id,attempt_number,workload_kind,cost_center_id,cost_center_name,cost_center_code,upstream_model,client_session_id,client_app) SELECT $1,w.id,$3,$4,$5,$6,$7,'started',$8,$9,$10,$11,w.cost_center_id,c.name,c.code,$12,$13,$14 FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$2 RETURNING id),
      r AS (INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,unbounded_cost,request_count) SELECT e.id,$2,$3,$4,$15,$8,date_trunc('minute',$8::timestamptz,'UTC'),date_trunc('month',$8::timestamptz,'UTC'),$16,'pending',$17,$18,$19,$20 FROM e RETURNING execution_id)
      INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd) SELECT $21,execution_id,'hold',$18 FROM r")
        .bind(record.id).bind(workspace).bind(key).bind(record.deployment_id).bind(&record.model).bind(&record.provider).bind(record.streamed).bind(now).bind(record.root_request_id).bind(record.attempt_number).bind(workload.kind.as_str()).bind(&record.upstream_model).bind(&record.client.session_id).bind(&record.client.app)
        .bind(price.as_ref().map(|p|p.id)).bind(lease).bind(tokens).bind(held).bind(held.is_none()).bind(request_count).bind(Uuid::new_v4())
        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if written != 1 {
        return Err(InferenceError::Storage);
    }
    timer.phase("write");
    tx.commit().await.map_err(storage)?;
    timer.phase("commit");
    Ok(())
}
/// Token reservation and monetary hold of one attempt under `price` (none
/// without a price): pure arithmetic over the immutable price row.
fn reservation_bounds(
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
enum LimitMode {
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
async fn enforce_limits(
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
    /// Metrics labels only (provider kind, configured public model).
    provider: String,
    public_model: String,
    /// Requests covered by one async batch attempt (0016); `None` = one.
    request_count: Option<i32>,
    /// Pinned price list (0021): `standard` or `batch`.
    price_tier: String,
}
async fn reservation(tx: &mut Tx<'_>, id: Uuid) -> Result<Reservation, InferenceError> {
    sqlx::query_as("SELECT r.workspace_id,r.deployment_id,r.price_id,r.state,r.input_tokens,r.output_tokens,r.billing_usage,e.workload_kind,r.reserved_tokens,r.meter_usage,r.output_image_variant,r.provider_cost_microusd,e.provider,e.public_model,r.request_count,r.price_tier FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=$1").bind(id).fetch_one(&mut **tx).await.map_err(storage)
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
    .map_err(storage)?
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
    .map_err(storage)?
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
    use crate::metrics::{AttemptObservation, METRICS, PhaseTimer, Settlement};
    let mut timer = PhaseTimer::start();
    let result = finish_unobserved(store, record, telemetry, &mut timer).await;
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
    let _queued = gate(&store.lock_gates.settlement).await;
    timer.phase("queue");
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    timer.phase("connect");
    lock(&mut tx).await?;
    timer.phase("locks");
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
    let changed=sqlx::query("UPDATE inference_executions SET state=$2,error_code=$3,input_tokens=$4,output_tokens=$5,billing_usage=$6,elapsed_ms=$7,completed_at=clock_timestamp(),meter_usage=$8,output_image_variant=$9,provider_cost_microusd=$10,finish_reason=$11,time_to_first_token_ms=$12,generation_ms=$13,reasoning_tokens=$14,reported_upstream_model=$15 WHERE id=$1 AND state='started'")
        .bind(record.id).bind(record.outcome.as_str()).bind(record.error.map(|e|e.code())).bind(input).bind(output).bind(&billing).bind(record.elapsed_ms.min(i64::MAX as u64)as i64).bind(&m.meters).bind(&m.variant).bind(m.provider_cost)
        .bind(telemetry.finish_reason.map(|f|f.as_str())).bind(ms(telemetry.time_to_first_token_ms)).bind(ms(telemetry.generation_ms)).bind(reasoning).bind(&reported_model).execute(&mut *tx).await.map_err(storage)?.rows_affected();
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
    timer.phase("write");
    tx.commit().await.map_err(storage)?;
    timer.phase("commit");
    Ok(Finished::Committed {
        provider: r.provider,
        model: r.public_model,
        settled: actual.is_some(),
        input,
        output,
    })
}
/// Expired-lease reservations reconciled per transaction.
const RECONCILE_BATCH: i64 = 50;
/// Marks up to `limit` expired pending reservations unknown (their holds are
/// retained) and their executions cancelled. Work is claimed in batches of
/// [`RECONCILE_BATCH`] with `FOR UPDATE SKIP LOCKED`, one transaction per
/// batch, so concurrent replicas never process (or wait on) the same rows and
/// each batch takes the installation lock once instead of once per row.
pub async fn reconcile_expired(store: &Store, limit: i64) -> Result<u64, InferenceError> {
    if !(1..=10_000).contains(&limit) {
        return Err(InferenceError::InvalidRequest);
    }
    let mut count = 0u64;
    while (count as i64) < limit {
        let batch = RECONCILE_BATCH.min(limit - count as i64);
        let _queued = gate(&store.lock_gates.settlement).await;
        let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
        lock(&mut tx).await?;
        let ids: Vec<Uuid> = sqlx::query_scalar("WITH due AS (SELECT execution_id FROM governance_reservations WHERE state='pending' AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at,execution_id LIMIT $1 FOR UPDATE SKIP LOCKED) UPDATE governance_reservations r SET state='unknown' FROM due WHERE r.execution_id=due.execution_id RETURNING r.execution_id")
            .bind(batch)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage)?;
        if !ids.is_empty() {
            let changed=sqlx::query("UPDATE inference_executions SET state='cancelled',error_code='lease_expired',finish_reason='cancelled',completed_at=clock_timestamp() WHERE id=ANY($1) AND state='started'").bind(&ids).execute(&mut *tx).await.map_err(storage)?.rows_affected();
            if changed != ids.len() as u64 {
                return Err(InferenceError::Storage);
            }
            // The ledger row `ledger()` writes for an unknown outcome without
            // usage: no amount, no observations.
            sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind) SELECT gen_random_uuid(),id,'unknown' FROM unnest($1::uuid[]) id")
                .bind(&ids)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            for id in &ids {
                // A crashed realtime session's open responses never reported usage.
                realtime::expire_open_responses(&mut tx, *id).await?;
            }
        }
        tx.commit().await.map_err(storage)?;
        count += ids.len() as u64;
        if (ids.len() as i64) < batch {
            break;
        }
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
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    lock(&mut tx).await?;
    let r = reservation(&mut tx, execution).await?;
    // Realtime sessions are valued per response (`realtime`); an aggregate
    // reconciliation could not reproduce that valuation, so it is refused.
    if r.workspace_id != workspace || r.workload_kind == WorkloadKind::Realtime.as_str() {
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
mod rates_tests;
/// Realtime session accounting: per-response windows on one reservation.
pub mod realtime;
#[cfg(all(test, feature = "integration-tests"))]
mod realtime_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod safety_tests;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(all(test, feature = "integration-tests"))]
mod totals_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod v3_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod workload_tests;
