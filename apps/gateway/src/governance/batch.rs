//! Governance of `/v1/batches` (0021; see `crate::jobs::batch` and
//! `docs/batches.md`).
//!
//! - [`admit_batch`]: one reservation for the whole batch, held at the sum of
//!   every line's own bound (input ceiling + the line's output maximum) at
//!   the applicable price list. It counts one "Jobs at once" slot and is
//!   exempt from requests/tokens per minute; every budget applies in full. A
//!   **native** batch is one upstream attempt: the reservation pins the
//!   deployment's price version and tier (`batch` when the version publishes
//!   batch price lines, else `standard`) and settles from the provider's
//!   aggregated usage. A **gateway-run** batch's reservation is an envelope
//!   that pins no price: it is never settled from usage.
//! - [`admit_line`]: each gateway-run line attempt gets its own execution and
//!   reservation at the standard price, like an interactive request, but its
//!   hold is *transferred* out of the envelope (the budget totals stay
//!   unchanged); only a hold the envelope can no longer cover is checked
//!   against budgets, so a batch whose lines overspend stops at the budget.
//! - [`close_batch`]: when a gateway-run batch ends, the envelope settles at
//!   zero, releasing what no line used. Lines keep their own settlement or
//!   retained unknown holds.
use std::collections::BTreeMap;

use super::*;

/// The price list a batch reservation is valued with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceTier {
    Standard,
    Batch,
}
impl PriceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Batch => "batch",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(Self::Standard),
            "batch" => Some(Self::Batch),
            _ => None,
        }
    }
}

/// The lines of one batch that route to one deployment.
#[derive(Clone)]
pub struct LineGroup {
    pub deployment: Deployment,
    /// Public model name the lines use.
    pub model: String,
    /// The batch endpoint's protocol (chat, responses, messages, embeddings).
    pub protocol: ApiProtocol,
    /// Output maximum → number of lines (0 for embeddings).
    pub outputs: BTreeMap<u32, u32>,
}
impl LineGroup {
    pub fn lines(&self) -> u64 {
        self.outputs.values().map(|n| u64::from(*n)).sum()
    }
}

/// What [`admit_batch`] reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchHold {
    pub held_microusd: i64,
    pub tier: PriceTier,
}

/// The deployment of `id` serving `model` to this workspace/key right now
/// (live catalog, key restrictions, enabled route and connection).
async fn current_deployment(
    store: &Store,
    tx: &mut Tx<'_>,
    workspace: Uuid,
    lineage: Uuid,
    id: Uuid,
    model: &str,
) -> Result<Deployment, InferenceError> {
    sqlx::query_as::<_,Deployment>(&format!("SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region,m.supported_protocols FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$2 AND m.public_name=$3 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$4) OR EXISTS(SELECT 1 FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$4 AND model_id=m.id)){}", catalog_share(store)))
        .bind(workspace).bind(id).bind(model).bind(lineage).fetch_optional(&mut **tx).await.map_err(storage)?.ok_or(InferenceError::ModelUnavailable)
}
fn same_target(current: &Deployment, expected: &Deployment) -> bool {
    current.id == expected.id
        && current.provider == expected.provider
        && current.upstream_model == expected.upstream_model
        && current.credential_ref == expected.credential_ref
        && current.endpoint == expected.endpoint
        && current.region == expected.region
        && current.supported_protocols == expected.supported_protocols
}
fn checked(a: i64, b: i64) -> Result<i64, InferenceError> {
    a.checked_add(b).ok_or(InferenceError::Configuration)
}
/// One line's `(reserved tokens, hold)` exactly as an interactive admission
/// of that line would compute it. Unpriced, unbounded or over-ceiling lines
/// are refused (a batch is always bounded).
fn line_bound(p: &Price, output: u32, generation: bool) -> Result<(i64, i64), InferenceError> {
    let zero_input_ok = p.input_token_limit == 0
        && p.pricing_version == 3
        && p.lines()?.0.input_tokens_inapplicable();
    if (p.input_token_limit < 1 && !zero_input_ok)
        || (generation && (output == 0 || i64::from(output) > p.output_token_limit))
        || (!generation && output != 0)
    {
        return Err(InferenceError::Configuration);
    }
    let held = p
        .bound(u64::from(output), &MeterUsage::default())?
        .ok_or(InferenceError::Configuration)?;
    Ok((checked(p.input_token_limit, i64::from(output))?, held))
}
fn group_bound(p: &Price, g: &LineGroup) -> Result<(i64, i64), InferenceError> {
    let generation = g.protocol.workload() == WorkloadKind::Generation;
    let (mut tokens, mut held) = (0i64, 0i64);
    for (output, count) in &g.outputs {
        let (t, h) = line_bound(p, *output, generation)?;
        let n = i64::from(*count);
        tokens = checked(
            tokens,
            t.checked_mul(n).ok_or(InferenceError::Configuration)?,
        )?;
        held = checked(held, h.checked_mul(n).ok_or(InferenceError::Configuration)?)?;
    }
    Ok((tokens, held))
}
fn serves(d: &Deployment, protocol: ApiProtocol, native: bool) -> bool {
    d.supported_protocols.iter().any(|p| p == protocol.as_str())
        // Models set up as "Batch" (0016) run Chat Completions natively.
        || (native
            && protocol == ApiProtocol::ChatCompletions
            && d.supported_protocols.iter().any(|p| p == "batches"))
}

/// Whether one line could be reserved for (validation preview).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinePreview {
    Ok,
    Unpriced,
    /// The output maximum exceeds the price's output ceiling.
    OutputLimit,
    /// A rate is unknown or a meter uncapped: no finite hold.
    Unbounded,
}

/// Read-only preview of one line at a deployment's current standard price
/// (a native batch's batch list shares the ceilings and is checked again at
/// admission).
pub async fn preview_line(
    store: &Store,
    deployment: Uuid,
    protocol: ApiProtocol,
    output: u32,
) -> Result<LinePreview, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    let price = latest_price(&mut tx, deployment).await?;
    tx.commit().await.map_err(storage)?;
    let Some(p) = price else {
        return Ok(LinePreview::Unpriced);
    };
    let generation = protocol.workload() == WorkloadKind::Generation;
    if generation && i64::from(output) > p.output_token_limit {
        return Ok(LinePreview::OutputLimit);
    }
    match line_bound(&p, output, generation) {
        Ok(_) => Ok(LinePreview::Ok),
        Err(InferenceError::Configuration) => Ok(LinePreview::Unbounded),
        Err(e) => Err(e),
    }
}

/// Whether a deployment's current price publishes a batch price list.
pub async fn batch_prices_published(
    store: &Store,
    deployment: Uuid,
) -> Result<bool, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    let price = latest_price(&mut tx, deployment).await?;
    tx.commit().await.map_err(storage)?;
    Ok(price.is_some_and(|p| p.has_batch_lines()))
}

/// Admit a batch: one execution (`batches` workload) and one reservation
/// holding every line's bound. `record.deployment_id` must be the first
/// group's deployment. A native batch has exactly one group.
pub async fn admit_batch(
    store: &Store,
    record: &ExecutionStart,
    groups: &[LineGroup],
    native: bool,
    lease_seconds: i64,
) -> Result<BatchHold, InferenceError> {
    let result = locks::retry_deadlocks!(
        "admission",
        admit_batch_unobserved(store, record, groups, native, lease_seconds).await
    );
    crate::metrics::observe_admission(&result.as_ref().map(|_| ()).map_err(|e| *e));
    result
}
async fn admit_batch_unobserved(
    store: &Store,
    record: &ExecutionStart,
    groups: &[LineGroup],
    native: bool,
    lease_seconds: i64,
) -> Result<BatchHold, InferenceError> {
    if !(1..=86_400).contains(&lease_seconds)
        || groups.is_empty()
        || (native && groups.len() != 1)
        || groups[0].deployment.id != record.deployment_id
    {
        return Err(InferenceError::Configuration);
    }
    let lines: u64 = groups.iter().map(LineGroup::lines).sum();
    let request_count = i32::try_from(lines)
        .ok()
        .filter(|n| (1..=crate::jobs::types::BATCH_MAX_REQUESTS as i32).contains(n))
        .ok_or(InferenceError::Configuration)?;
    let workspace = record.principal.workspace_id;
    let _queued = gate(&store.lock_gates.admission).await;
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    admission_prefix(store, &mut tx, &record.principal).await?;
    let lineage = authorize(store, &mut tx, &record.principal).await?;
    let (mut tokens, mut held) = (0i64, 0i64);
    let mut pinned: Option<(Uuid, PriceTier)> = None;
    for g in groups {
        let current = current_deployment(
            store,
            &mut tx,
            workspace,
            lineage,
            g.deployment.id,
            &g.model,
        )
        .await?;
        if !same_target(&current, &g.deployment) || !serves(&current, g.protocol, native) {
            return Err(InferenceError::Configuration);
        }
        let p = latest_price(&mut tx, g.deployment.id)
            .await?
            .ok_or(InferenceError::Configuration)?;
        let tier = if native && p.has_batch_lines() {
            PriceTier::Batch
        } else {
            PriceTier::Standard
        };
        let id = p.id;
        let p = p.tiered(tier.as_str())?;
        let (t, h) = group_bound(&p, g)?;
        tokens = checked(tokens, t)?;
        held = checked(held, h)?;
        if native {
            pinned = Some((id, tier));
        }
    }
    let now: DateTime<Utc> = store.admission_now(&mut tx).await.map_err(storage)?;
    let lease = now
        .checked_add_signed(chrono::TimeDelta::seconds(lease_seconds))
        .ok_or(InferenceError::Configuration)?;
    lock_rows(
        store,
        &mut tx,
        &[locks::Touch {
            workspace,
            api_key: record.principal.key_id,
            at: now,
        }],
        true,
    )
    .await?;
    enforce_limits(
        &mut tx,
        workspace,
        lineage,
        now,
        LimitMode::Job,
        Some(tokens),
        Some(held),
        false,
    )
    .await?;
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,root_request_id,attempt_number,workload_kind,cost_center_id,cost_center_name,cost_center_code,upstream_model,client_session_id,client_app) SELECT $1,w.id,$3,$4,$5,$6,false,'started',$7,$8,$9,'batches',w.cost_center_id,c.name,c.code,$10,$11,$12 FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$2")
        .bind(record.id).bind(workspace).bind(record.principal.key_id).bind(record.deployment_id).bind(&record.model).bind(&record.provider).bind(now).bind(record.root_request_id).bind(record.attempt_number).bind(&record.upstream_model).bind(&record.client.session_id).bind(&record.client.app).execute(&mut *tx).await.map_err(storage)?;
    let tier = pinned.map_or(PriceTier::Standard, |p| p.1);
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,unbounded_cost,request_count,price_tier) VALUES($1,$2,$3,$4,$5,$6,date_trunc('minute',$6::timestamptz,'UTC'),date_trunc('month',$6::timestamptz,'UTC'),$7,'pending',$8,$9,false,$10,$11)")
        .bind(record.id).bind(workspace).bind(record.principal.key_id).bind(record.deployment_id).bind(pinned.map(|p| p.0)).bind(now).bind(lease).bind(tokens).bind(held).bind(request_count).bind(tier.as_str()).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        record.id,
        "hold",
        Some(held),
        Usage::default(),
        None,
        None,
    )
    .await?;
    tx.commit().await.map_err(storage)?;
    Ok(BatchHold {
        held_microusd: held,
        tier,
    })
}

/// The gateway-run batch a line attempt belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineContext {
    /// The batch's envelope execution (its reservation holds the ceiling).
    pub envelope: Uuid,
    pub job: Uuid,
}

/// Admit one gateway-run line attempt (see the module docs). Errors: the
/// usual admission errors; `Configuration` when the batch's envelope is no
/// longer pending (closed or reconciled), so no line may start.
pub async fn admit_line(
    store: &Store,
    batch: LineContext,
    record: &ExecutionStart,
    workload: &WorkloadAdmission,
    lease_seconds: i64,
    expected: &Deployment,
) -> Result<(), InferenceError> {
    let result = locks::retry_deadlocks!(
        "admission",
        admit_line_unobserved(store, batch, record, workload, lease_seconds, expected).await
    );
    crate::metrics::observe_admission(&result);
    result
}
async fn admit_line_unobserved(
    store: &Store,
    batch: LineContext,
    record: &ExecutionStart,
    workload: &WorkloadAdmission,
    lease_seconds: i64,
    expected: &Deployment,
) -> Result<(), InferenceError> {
    if !(1..=86_400).contains(&lease_seconds)
        || !matches!(
            workload.kind,
            WorkloadKind::Generation | WorkloadKind::Embeddings
        )
    {
        return Err(InferenceError::Configuration);
    }
    let workspace = record.principal.workspace_id;
    let _queued = gate(&store.lock_gates.admission).await;
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    admission_prefix(store, &mut tx, &record.principal).await?;
    let lineage = authorize(store, &mut tx, &record.principal).await?;
    let envelope: Option<(String, Option<i64>, Uuid, DateTime<Utc>)> = sqlx::query_as("SELECT r.state,r.held_microusd,r.api_key_id,r.admitted_at FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.execution_id=$1 AND r.workspace_id=$2 AND j.id=$3 AND j.batch_mode='gateway' FOR UPDATE OF r")
        .bind(batch.envelope).bind(workspace).bind(batch.job).fetch_optional(&mut *tx).await.map_err(storage)?;
    let Some((state, available, envelope_key, envelope_at)) = envelope else {
        return Err(InferenceError::Configuration);
    };
    if state != "pending" {
        return Err(InferenceError::Configuration);
    }
    let current = current_deployment(
        store,
        &mut tx,
        workspace,
        lineage,
        record.deployment_id,
        &record.model,
    )
    .await?;
    if current.provider != record.provider
        || !same_target(&current, expected)
        || !current
            .supported_protocols
            .iter()
            .any(|p| ApiProtocol::parse(p).is_some_and(|p| p.workload() == workload.kind))
    {
        return Err(InferenceError::Configuration);
    }
    let p = latest_price(&mut tx, record.deployment_id)
        .await?
        .ok_or(InferenceError::Configuration)?;
    let output = match workload.output {
        OutputReservation::None => 0,
        OutputReservation::Requested(Some(n)) => n,
        _ => return Err(InferenceError::Configuration),
    };
    let (tokens, held) = line_bound(&p, output, workload.kind == WorkloadKind::Generation)?;
    let transfer = held.min(available.unwrap_or(0).max(0));
    let now: DateTime<Utc> = store.admission_now(&mut tx).await.map_err(storage)?;
    let lease = now
        .checked_add_signed(chrono::TimeDelta::seconds(lease_seconds))
        .ok_or(InferenceError::Configuration)?;
    // The line's buckets (now) and the envelope's (its admission time): the
    // insert and the envelope transfer below change both.
    lock_rows(
        store,
        &mut tx,
        &[
            locks::Touch {
                workspace,
                api_key: record.principal.key_id,
                at: now,
            },
            locks::Touch {
                workspace,
                api_key: envelope_key,
                at: envelope_at,
            },
        ],
        true,
    )
    .await?;
    enforce_limits(
        &mut tx,
        workspace,
        lineage,
        now,
        LimitMode::BatchLine,
        Some(tokens),
        Some(held - transfer),
        false,
    )
    .await?;
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,root_request_id,attempt_number,workload_kind,cost_center_id,cost_center_name,cost_center_code,upstream_model,client_session_id,client_app,batch_job_id) SELECT $1,w.id,$3,$4,$5,$6,$7,'started',$8,$9,$10,$11,w.cost_center_id,c.name,c.code,$12,$13,$14,$15 FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$2")
        .bind(record.id).bind(workspace).bind(record.principal.key_id).bind(record.deployment_id).bind(&record.model).bind(&record.provider).bind(record.streamed).bind(now).bind(record.root_request_id).bind(record.attempt_number).bind(workload.kind.as_str()).bind(&record.upstream_model).bind(&record.client.session_id).bind(&record.client.app).bind(batch.job).execute(&mut *tx).await.map_err(storage)?;
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,unbounded_cost) VALUES($1,$2,$3,$4,$5,$6,date_trunc('minute',$6::timestamptz,'UTC'),date_trunc('month',$6::timestamptz,'UTC'),$7,'pending',$8,$9,false)")
        .bind(record.id).bind(workspace).bind(record.principal.key_id).bind(record.deployment_id).bind(p.id).bind(now).bind(lease).bind(tokens).bind(held).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        record.id,
        "hold",
        Some(held),
        Usage::default(),
        None,
        None,
    )
    .await?;
    if transfer > 0 {
        sqlx::query("UPDATE governance_reservations SET held_microusd=held_microusd-$2 WHERE execution_id=$1 AND state='pending' AND held_microusd>=$2")
            .bind(batch.envelope)
            .bind(transfer)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
    }
    tx.commit().await.map_err(storage)
}

/// Settle a batch reservation at zero: a gateway-run batch's envelope when
/// the batch ends (the part of the ceiling no line used is released), or a
/// native batch cancelled before anything was submitted (`cancelled`).
/// Callers guarantee no upstream work is covered by it. Idempotent; a
/// reservation that is no longer pending (lease reconciled) keeps its hold.
pub async fn close_batch(
    store: &Store,
    envelope: Uuid,
    elapsed_ms: u64,
    cancelled: bool,
) -> Result<bool, InferenceError> {
    let _queued = gate(&store.lock_gates.settlement).await;
    locks::retry_deadlocks!(
        "settlement",
        close_batch_once(store, envelope, elapsed_ms, cancelled).await
    )
}
async fn close_batch_once(
    store: &Store,
    envelope: Uuid,
    elapsed_ms: u64,
    cancelled: bool,
) -> Result<bool, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    settlement_prefix(store, &mut tx).await?;
    if scoped(store) {
        // The envelope row, then its totals rows.
        let row: Option<(Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as("SELECT workspace_id,api_key_id,admitted_at FROM governance_reservations WHERE execution_id=$1 AND state='pending' FOR UPDATE")
            .bind(envelope)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        if let Some((workspace, api_key, at)) = row {
            lock_rows(
                store,
                &mut tx,
                &[locks::Touch {
                    workspace,
                    api_key,
                    at,
                }],
                false,
            )
            .await?;
        }
    }
    let changed = sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=0,input_tokens=0,output_tokens=0 WHERE execution_id=$1 AND state='pending'")
        .bind(envelope)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
    if changed == 0 {
        tx.commit().await.map_err(storage)?;
        return Ok(false);
    }
    let changed = sqlx::query("UPDATE inference_executions SET state=$3,input_tokens=0,output_tokens=0,elapsed_ms=$2,completed_at=clock_timestamp(),finish_reason=CASE WHEN $3='cancelled' THEN 'cancelled' ELSE 'stop' END WHERE id=$1 AND state='started' AND workload_kind='batches'")
        .bind(envelope)
        .bind(elapsed_ms.min(i64::MAX as u64) as i64)
        .bind(if cancelled { "cancelled" } else { "succeeded" })
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    ledger(
        &mut tx,
        envelope,
        "settlement",
        Some(0),
        Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            ..Usage::default()
        },
        None,
        None,
    )
    .await?;
    tx.commit().await.map_err(storage)?;
    Ok(true)
}

/// Cost so far of a batch, in micro-USD: settled actual plus active holds of
/// its line attempts (gateway-run) or its single reservation (native), and
/// whether any of it is still unknown.
pub async fn batch_cost(store: &Store, job: Uuid) -> Result<(i64, i64, bool), InferenceError> {
    let row: (Option<i64>, Option<i64>, bool) = sqlx::query_as("WITH r AS (SELECT r.state,r.actual_microusd,r.held_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1 UNION ALL SELECT r.state,r.actual_microusd,r.held_microusd FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE j.id=$1 AND (j.batch_mode IS DISTINCT FROM 'gateway')) SELECT coalesce(sum(actual_microusd) FILTER(WHERE state='settled'),0)::bigint,coalesce(sum(held_microusd) FILTER(WHERE state<>'settled'),0)::bigint,coalesce(bool_or(state='unknown'),false) FROM r")
        .bind(job)
        .fetch_one(&store.pool)
        .await
        .map_err(storage)?;
    Ok((row.0.unwrap_or(0), row.1.unwrap_or(0), row.2))
}
