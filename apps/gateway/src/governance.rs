//! Durable per-attempt accounting. Lock order: [`locks`] (catalog lock, then
//! scoped authority locks, then the reservation row, then totals/counter rows
//! in primary-key order; `GATEWAY_ADMISSION_MODE=global` takes the former
//! installation row lock after the catalog lock instead).
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
fn storage(error: sqlx::Error) -> InferenceError {
    storage_at(crate::db::accounting_failure::storage_error, error)
}
/// As [`storage`], for a failed start of an accounting transaction (#389).
fn begin_storage(error: sqlx::Error) -> InferenceError {
    storage_at(crate::db::accounting_failure::begin_error, error)
}
/// As [`storage`], for a statement run straight on the pool, outside a
/// transaction: without a connection it is a failed start (#389).
fn pool_storage(error: sqlx::Error) -> InferenceError {
    storage_at(crate::db::accounting_failure::pool_statement_error, error)
}
fn storage_at(signal: fn(&sqlx::Error), error: sqlx::Error) -> InferenceError {
    #[cfg(test)]
    crate::db::diagnostics::observe_sqlx(crate::db::diagnostics::Stage::GovernanceStorage, &error);
    // A deadlock victim may be re-run (`locks::retry_deadlocks`), which
    // reports the failure itself once the re-runs are used up (#389).
    if !locks::note(&error) {
        signal(&error);
    }
    InferenceError::Storage
}
/// A private submodule so [`SharedEligibilityCommitted`]'s own tuple field
/// (and therefore `SharedEligibilityCommitted(())` construction) is scoped
/// to this submodule alone, not to the whole `governance` module tree
/// (#98 PR 2 review item 3). A private field on a struct defined directly
/// in `governance.rs` would have been constructible from *every* submodule
/// declared there (`batch`, `jobs`, `locks`, `pressure`, `rates`, `totals`,
/// `realtime`): Rust grants a private item's defining module *and all of
/// that module's descendants* access, and those submodules are all
/// descendants of `governance`. Nesting one level down shrinks that
/// descendant set to `proof` itself, which has none.
mod proof {
    /// Proof, for [`crate::telemetry::mark_shared`], that this exact
    /// module's own commit branch of `admit_unobserved` resolved
    /// shared-workspace trace eligibility. The struct's *type name* is
    /// `pub(crate)` only so it can be re-exported from `governance` (see
    /// below) and named in `telemetry::mark_shared`'s signature; its
    /// unnamed tuple field has ordinary default (private) visibility,
    /// which Rust scopes to this `proof` submodule alone (it has no
    /// descendants), so `SharedEligibilityCommitted(())` can only ever be
    /// written here. `committed_shared` is `pub(super)` - the narrowest
    /// visibility that still lets `governance.rs`'s `admit_unobserved`
    /// (this module's parent) call it - and is the one place that does.
    pub(crate) struct SharedEligibilityCommitted(());
    pub(super) fn committed_shared() -> SharedEligibilityCommitted {
        SharedEligibilityCommitted(())
    }
}
pub(crate) use proof::SharedEligibilityCommitted;
/// Bounded in-process queues in front of the governance transactions, one for
/// admissions and one for settlements/reconciliation. In the global mode work
/// under the installation lock is serialized anyway, so letting every
/// concurrent request hold a pooled connection while it waits adds no
/// throughput: it starves authentication, routing and settlement of
/// connections (the load test saw 503s and unsettled streams at 200
/// concurrent requests on a 10-connection pool). There each queue lets at
/// most a quarter of the pool wait on the database lock. Scoped admissions of
/// different scopes run in parallel, so up to half the pool may admit and
/// three tenths settle; the rest stays free for authentication and routing.
/// Other callers wait here without a connection. Settlements never queue
/// behind admissions in-process, so holds are released promptly.
pub struct LockGates {
    admission: tokio::sync::Semaphore,
    settlement: tokio::sync::Semaphore,
}
impl LockGates {
    pub(crate) fn for_pool(max_connections: u32, mode: locks::AdmissionMode) -> Self {
        let pool = max_connections as usize;
        let (admission, settlement) = match mode {
            locks::AdmissionMode::Global => (pool / 4, pool / 4),
            // P3 load test: with a quarter of the pool, one replica's
            // settlements queued in process at 1000/s (2 permits of 10).
            locks::AdmissionMode::Scoped => (pool / 2, pool * 3 / 10),
        };
        Self {
            admission: tokio::sync::Semaphore::new(admission.max(1)),
            settlement: tokio::sync::Semaphore::new(settlement.max(1)),
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
/// The global-mode prefix: the shared catalog lock, then the installation row.
async fn lock(tx: &mut Tx<'_>) -> Result<(), InferenceError> {
    catalog_lock(tx).await?;
    installation_lock(tx).await
}
fn scoped(store: &Store) -> bool {
    store.admission_mode == locks::AdmissionMode::Scoped
}
/// Lock prefix of a new admission (or admission-like growth) of `principal`:
/// scoped, the shared catalog and authority locks (`locks::admission`);
/// global, the catalog lock and the installation row.
/// Returns the scoped authority-lock wait (zero in the global mode), also
/// observed as `gateway_scope_lock_wait_seconds{lock="authority"}`.
async fn admission_prefix(
    store: &Store,
    tx: &mut Tx<'_>,
    principal: &crate::auth::Principal,
) -> Result<std::time::Duration, InferenceError> {
    if scoped(store) {
        let wait = crate::metrics::METRICS.scope_lock_wait("authority", "admission");
        locks::admission(tx, principal).await.map_err(storage)?;
        Ok(wait.elapsed())
    } else {
        lock(tx).await?;
        Ok(std::time::Duration::ZERO)
    }
}
/// Live authorization of `principal` after [`admission_prefix`]: its key
/// lineage, or `Unauthenticated` when the key is no longer valid (revoked,
/// disabled, expired, or its user, membership, service account or workspace
/// no longer active: exactly the conditions under which live authentication
/// refuses the key), in which case this replica's cached entries of the key
/// are dropped at once. Model availability is checked separately
/// (`ModelUnavailable`). Scoped admission reads without row locks (the
/// shared authority locks exclude concurrent revocation); the global mode
/// keeps its `FOR SHARE` row locks.
/// Returns the key lineage and whether the admitting workspace is shared
/// (`team`/`project`): the only source of trace-export eligibility
/// (`admit_unobserved`'s commit branch), live-revalidated here under this
/// transaction's authority locks, never cached or client-asserted.
async fn authorize(
    store: &Store,
    tx: &mut Tx<'_>,
    principal: &crate::auth::Principal,
) -> Result<(Uuid, bool), InferenceError> {
    match crate::auth::revalidate_admission_with(tx, principal, !scoped(store))
        .await
        .map_err(storage)?
    {
        Some(lineage) => Ok(lineage),
        None => {
            store.caches.forget_key(principal.key_id);
            Err(InferenceError::Unauthenticated)
        }
    }
}
/// Settlement prefix: global, the catalog lock and the installation row;
/// scoped, nothing (the reservation row lock taken by the next read
/// serializes settlement, reconciliation and growth of one reservation).
async fn settlement_prefix(store: &Store, tx: &mut Tx<'_>) -> Result<(), InferenceError> {
    if scoped(store) {
        Ok(())
    } else {
        lock(tx).await
    }
}
/// Scoped mode: lock the totals rows (and with `counters` the minute and
/// in-flight rows) the next writes change, in canonical order. A no-op in the
/// global mode, where the installation row serializes everything. Returns the
/// wait, observed as `gateway_scope_lock_wait_seconds{lock="rows"}` with path
/// `admission` (counter rows: new consumption) or `settlement`.
async fn lock_rows(
    store: &Store,
    tx: &mut Tx<'_>,
    touches: &[locks::Touch],
    counters: bool,
) -> Result<std::time::Duration, InferenceError> {
    if !scoped(store) {
        return Ok(std::time::Duration::ZERO);
    }
    let path = if counters { "admission" } else { "settlement" };
    let wait = crate::metrics::METRICS.scope_lock_wait("rows", path);
    locks::rows(tx, touches, counters).await.map_err(storage)?;
    Ok(wait.elapsed())
}
/// The `FOR SHARE` clause of the global mode's catalog row reads (scoped
/// admission relies on the shared catalog lock, which every catalog writer
/// takes exclusively).
fn catalog_share(store: &Store) -> &'static str {
    if scoped(store) {
        ""
    } else {
        " FOR SHARE OF d,p,m"
    }
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
/// Budget consumption of one scope (a workspace, or one key lineage inside it)
/// in the current window of a period: settled actual plus active
/// pending/unknown holds by admission time, and whether unresolved
/// unbounded/unpriced usage makes the sum a lower bound. Read from the
/// maintained totals (migration 0015) that admission also uses.
pub(crate) use totals::budget_consumption;
/// Admission entry points, reservation bounds and limit enforcement.
mod admission;
/// Batch admission, line admission and closing (`crate::jobs::batch`, 0021).
pub mod batch;
/// Lease extension and bound preview for async jobs (`crate::jobs`).
pub mod jobs;
/// Canonical lock order, authority scope locks and the admission mode.
pub mod locks;
/// Budget periods and the applicable policy and budget rows.
mod policy;
pub mod pressure;
/// Price versions, bounds and cost arithmetic.
mod pricing;
/// Maintained per-scope, per-minute rate and in-flight counters.
pub mod rates;
/// Expired-lease and evidence-backed reconciliation.
mod reconcile;
/// Settlement, valuation and ledger writes of one attempt.
mod settlement;
/// Maintained per-scope, per-period budget totals and their consistency check.
pub mod totals;
pub(crate) use admission::generation_bound;
use admission::{LimitMode, enforce_limits, reservation_bounds};
pub use admission::{
    admit, admit_embeddings_for_deployment, admit_for_deployment, admit_generation,
    admit_workload_for_deployment,
};
pub(crate) use policy::BUDGETS;
pub use policy::BudgetPeriod;
use policy::{Budget, POLICIES, Policy};
#[cfg(test)]
use pricing::cost;
pub use pricing::price_meters;
use pricing::{
    PRICE_COLUMNS, Price, checked_cost, latest_price, price_version, require_native_cache_price,
};
pub use reconcile::{ReconcilingActor, reconcile_expired, resolve_usage};
pub(crate) use settlement::validate_token_usage;
use settlement::{
    billing_json, cache_bound_violated, ledger, meter_evidence, pinned_value, reservation,
    usage_values, workload_usage,
};
pub use settlement::{finish, finish_with_telemetry};
#[allow(clippy::too_many_arguments)]
#[cfg_attr(
    not(test),
    expect(clippy::too_many_lines, clippy::cognitive_complexity, reason = "#499")
)]
async fn admit_unobserved(
    store: &Store,
    record: &ExecutionStart,
    mut workload: WorkloadAdmission,
    lease_seconds: i64,
    expected: Option<&Deployment>,
    output_limit: u32,
    generation_request: Option<&ChatRequest>,
    timer: &mut crate::metrics::PhaseTimer,
) -> Result<crate::inference::repository::GenerationAdmission, InferenceError> {
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(InferenceError::Configuration);
    }
    let workspace = record.principal.workspace_id;
    let key = record.principal.key_id;
    let _queued = gate(&store.lock_gates.admission).await;
    timer.phase("queue");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionQueue);
    let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
    timer.phase("connect");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionConnect);
    // Catalog configuration (deployments, providers, models and their
    // append-only prices) changes only under the exclusive catalog lock, so
    // once it is held shared the latest price and the reservation bounds are
    // fixed for this transaction. Authorization, entitlement and policies
    // change under the exclusive authority locks of their scope (scoped) or
    // the installation lock (global), both held from here to commit. Bound
    // errors are reported after the checks below, in the former order.
    // Scoped: the latest price id is read with the deployment check below
    // and its immutable row comes from the price cache (one round trip less).
    let mut lock_wait = std::time::Duration::ZERO;
    let read_price = if scoped(store) {
        lock_wait += admission_prefix(store, &mut tx, &record.principal).await?;
        timer.phase("locks");
        // Scoped admission takes the shared catalog and authority locks
        // together (locks::admission): one real boundary, named for its
        // dominant role (authority locks of workspace type, workspace,
        // user and key lineage).
        crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionAuthorityLocks);
        None
    } else {
        catalog_lock(&mut tx).await?;
        let price = latest_price(&mut tx, record.deployment_id).await?;
        timer.phase("price");
        crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionCatalogLocks);
        installation_lock(&mut tx).await?;
        timer.phase("locks");
        crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionAuthorityLocks);
        Some(price)
    };
    let (lineage, shared) = authorize(store, &mut tx, &record.principal).await?;
    // The deployment check also reads the admission clock and the latest
    // price id (one round trip).
    let row=sqlx::query(&format!("SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region,m.supported_protocols,clock_timestamp() AS admission_now,(SELECT x.id FROM deployment_prices x WHERE x.deployment_id=d.id ORDER BY x.created_at DESC,x.id DESC LIMIT 1) AS latest_price_id FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$2 AND m.public_name=$3 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$4) OR EXISTS(SELECT 1 FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$4 AND model_id=m.id)){}", catalog_share(store)))
        .bind(workspace).bind(record.deployment_id).bind(&record.model).bind(lineage).fetch_optional(&mut *tx).await.map_err(storage)?.ok_or(InferenceError::ModelUnavailable)?;
    let current = <Deployment as sqlx::FromRow<_>>::from_row(&row).map_err(storage)?;
    let database_now: DateTime<Utc> = sqlx::Row::try_get(&row, "admission_now").map_err(storage)?;
    let price = match read_price {
        Some(price) => price,
        None => {
            match sqlx::Row::try_get::<Option<Uuid>, _>(&row, "latest_price_id").map_err(storage)? {
                Some(id) => Some(price_version(&mut tx, id, record.deployment_id).await?),
                None => None,
            }
        }
    };
    let admission = match workload.output {
        OutputReservation::Requested(requested) if workload.kind == WorkloadKind::Generation => {
            generation_bound(
                price.as_ref().map(|p| p.output_token_limit),
                requested,
                output_limit,
            )
        }
        _ => Ok(crate::inference::repository::GenerationAdmission {
            max_output_tokens: None,
        }),
    };
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
    // Only the live, validated native deployment forwards these hints. Check
    // its immutable pinned price under the catalog lock, before any execution,
    // reservation, counters or totals are written. Translated routes drop hints.
    if current.provider == "anthropic"
        && let Some(options) = generation_request.and_then(|r| r.messages_options.as_deref())
    {
        require_native_cache_price(price.as_ref(), &options.cache_controls)?;
    }
    let now = store.admission_override().unwrap_or(database_now);
    let lease = now
        .checked_add_signed(chrono::TimeDelta::seconds(lease_seconds))
        .ok_or(InferenceError::Configuration)?;
    let admission = admission?;
    if workload.kind == WorkloadKind::Generation {
        workload.output = OutputReservation::Requested(admission.max_output_tokens);
    }
    let (tokens, held) = reservation_bounds(price.as_ref(), &workload)?;
    if held.is_none()
        && current.provider == "anthropic"
        && generation_request.is_some_and(|r| {
            r.messages_options
                .as_ref()
                .is_some_and(|o| !o.cache_controls.is_empty())
        })
    {
        // A cache-enabled native request needs a finite conservative hold even
        // without a budget; unrelated unknown cache rates cannot erase it.
        return Err(InferenceError::InvalidField("cache_control"));
    }
    // A pricing-v3 price that cannot bound this attempt (a possibly-used meter
    // without a line, an explicit unknown, or no `max_units`) is refused under
    // any budget with `price_unbounded`. v1/v2 keep their legacy
    // configuration error.
    let v3_unbounded = price.as_ref().is_some_and(|p| p.pricing_version == 3) && held.is_none();
    // Async jobs (video, batch) are exempt from requests/tokens-per-minute
    // limits and are counted by "jobs at once"; budgets apply in full.
    let job = matches!(workload.kind, WorkloadKind::Videos | WorkloadKind::Batches);
    timer.phase("read");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionRead);
    // Scoped: hold this workspace's and lineage's totals and counter rows
    // (the ones the write below changes) before reading them.
    lock_wait += lock_rows(
        store,
        &mut tx,
        &[locks::Touch {
            workspace,
            api_key: key,
            at: now,
        }],
        true,
    )
    .await?;
    timer.phase("lock_rows");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionTotalsLocks);
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
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionLimits);
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
      INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,admitted_at) SELECT $21,execution_id,'hold',$18,$8 FROM r")
        .bind(record.id).bind(workspace).bind(key).bind(record.deployment_id).bind(&record.model).bind(&record.provider).bind(record.streamed).bind(now).bind(record.root_request_id).bind(record.attempt_number).bind(workload.kind.as_str()).bind(&record.upstream_model).bind(&record.client.session_id).bind(&record.client.app)
        .bind(price.as_ref().map(|p|p.id)).bind(lease).bind(tokens).bind(held).bind(held.is_none()).bind(request_count).bind(Uuid::now_v7())
        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if written != 1 {
        return Err(InferenceError::Storage);
    }
    timer.phase("write");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionWrite);
    tx.commit().await.map_err(storage)?;
    timer.phase("commit");
    crate::telemetry::mark_phase(crate::telemetry::SpanName::AdmissionCommit);
    // The only production trigger of trace-export eligibility: a committed
    // admission for a shared (team/project) workspace, carrying the public
    // alias admission itself resolved (record.model matched m.public_name
    // above). Never personal, never client text, never before this commit
    // has actually succeeded.
    if shared {
        crate::telemetry::mark_shared(
            proof::committed_shared(),
            record.root_request_id,
            &record.model,
        );
    }
    // Per-scope ceiling signal (`admission_ceiling` alerts): committed
    // interactive admissions (the ones `rate_minute_counters` counts) that
    // waited at least 10 ms for their scope locks.
    if !job && scoped(store) {
        pressure::record(now, workspace, lineage, lock_wait);
    }
    Ok(admission)
}
#[cfg(all(test, feature = "integration-tests"))]
mod accounting_failure_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod anthropic_cache_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod audio_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod generation_defaults_tests;
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
#[cfg(test)]
mod diagnostic_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod image_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod locks_tests;
#[cfg(test)]
mod period_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod rates_tests;
/// Realtime session accounting: per-response windows on one reservation.
pub mod realtime;
#[cfg(all(test, feature = "integration-tests"))]
mod realtime_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod reconcile_token_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod safety_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod telemetry_tests;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(all(test, feature = "integration-tests"))]
mod totals_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod v3_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod workload_tests;
