//! Explicit terminal gateway envelope resolution. Line reservations own charges.
use super::free_price::{MAX_CANDIDATES, Resolve};
use super::*;
use crate::governance::locks::{self, Scope};
use gateway_datetime_compat::UtcInstant;
use sqlx::{Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
mod paging;

#[derive(sqlx::FromRow)]
struct Candidate {
    job_id: Uuid,
    execution_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    admitted_at: UtcInstant,
    held_microusd: i64,
}
// A terminal gateway record proves the envelope covers no provider attempt.
// Every admitted line must still have its own reservation; no missing charge
// can be concealed by releasing the envelope. The job lock fences new lines.
// Keep every history lookup behind its UUID probe. Large selected-key sets
// can otherwise make PostgreSQL reorder the joins into repeated full-history
// scans, even though the result page is bounded. OFFSET 0 preserves the
// correlated probes without changing eligibility or composite-key matching.
const CANDIDATES: &str = "SELECT j.id AS job_id,r.execution_id,r.workspace_id,r.api_key_id,r.admitted_at,r.held_microusd
FROM async_jobs j
CROSS JOIN LATERAL (
 SELECT r.execution_id,r.workspace_id,r.api_key_id,r.deployment_id,r.admitted_at,r.held_microusd,r.state,r.price_id,r.price_tier,r.unbounded_cost
 FROM governance_reservations r WHERE r.execution_id=j.execution_id OFFSET 0
) r
CROSS JOIN LATERAL (
 SELECT e.state,e.workload_kind,e.batch_job_id FROM inference_executions e
 WHERE e.id=r.execution_id AND e.started_at=r.admitted_at OFFSET 0
) e
WHERE j.kind='batch' AND j.batch_mode='gateway' AND j.state IN ('completed','failed','cancelled','expired') AND j.completed_at IS NOT NULL
AND j.upstream_id IS NULL AND j.submit_started_at IS NULL
AND r.workspace_id=j.workspace_id AND r.api_key_id=j.api_key_id AND r.deployment_id=j.deployment_id
AND r.state='unknown' AND r.price_id IS NULL AND r.price_tier='standard' AND r.held_microusd IS NOT NULL AND NOT r.unbounded_cost
AND e.state<>'started' AND e.workload_kind='batches' AND e.batch_job_id IS NULL
AND NOT EXISTS(SELECT FROM inference_executions other WHERE other.id=j.execution_id AND other.started_at<>r.admitted_at OFFSET 0)
AND NOT EXISTS(SELECT FROM governance_reservations other WHERE other.execution_id=j.execution_id AND other.admitted_at<>r.admitted_at OFFSET 0)
AND ($1::timestamptz IS NULL OR r.admitted_at >= $1) AND ($2::timestamptz IS NULL OR r.admitted_at < $2)
AND ($3::uuid IS NULL OR r.workspace_id=$3) AND ($4::uuid IS NULL OR r.deployment_id=$4)
AND ($5::uuid[] IS NULL OR (r.execution_id,r.admitted_at) IN (SELECT * FROM unnest($5::uuid[],$6::timestamptz[])))
ORDER BY r.admitted_at,r.execution_id LIMIT 10001 OFFSET $7";
async fn candidates(
    tx: &mut Transaction<'_, Postgres>,
    p: &Resolve,
    selected: Option<&[Candidate]>,
) -> Result<Vec<Candidate>, ApiError> {
    let bounds = p.bounds()?;
    let ids = selected.map(|rows| rows.iter().map(|r| r.execution_id).collect::<Vec<_>>());
    let times = selected.map(|rows| rows.iter().map(|r| r.admitted_at).collect::<Vec<_>>());
    Ok(sqlx::query_as(CANDIDATES)
        .bind(bounds.map(|b| b.0))
        .bind(bounds.map(|b| b.1))
        .bind(p.workspace_id)
        .bind(p.deployment_id)
        .bind(ids)
        .bind(times)
        .bind(if selected.is_none() { p.offset()? } else { 0 })
        .fetch_all(&mut **tx)
        .await?)
}
#[derive(sqlx::FromRow)]
struct Line {
    batch_job_id: Uuid,
    settled: i64,
    held: i64,
    pending: i64,
    unknown: i64,
    unbounded: i64,
}
#[derive(Default, Clone)]
struct Evidence {
    envelope_held: i64,
    settled: i64,
    held: i64,
    pending: i64,
    unknown: i64,
    unbounded: i64,
}
fn add(a: i64, b: i64) -> Result<i64, ApiError> {
    a.checked_add(b).ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Accounting total exceeds supported range",
    ))
}
impl Evidence {
    fn json(&self) -> Value {
        json!({"envelope_held_microusd":self.envelope_held.to_string(),"line_settled_microusd":self.settled.to_string(),"line_held_microusd":self.held.to_string(),"line_pending_count":self.pending.to_string(),"line_unknown_count":self.unknown.to_string(),"line_unbounded_count":self.unbounded.to_string()})
    }
    fn include(&mut self, other: &Self) -> Result<(), ApiError> {
        self.envelope_held = add(self.envelope_held, other.envelope_held)?;
        self.settled = add(self.settled, other.settled)?;
        self.held = add(self.held, other.held)?;
        self.pending = add(self.pending, other.pending)?;
        self.unknown = add(self.unknown, other.unknown)?;
        self.unbounded = add(self.unbounded, other.unbounded)?;
        Ok(())
    }
}
async fn locked(
    tx: &mut Transaction<'_, Postgres>,
    initial: &[Candidate],
) -> Result<(Vec<Line>, BTreeSet<Uuid>), ApiError> {
    let jobs: Vec<_> = initial.iter().map(|r| r.job_id).collect();
    let executions: Vec<_> = initial.iter().map(|r| r.execution_id).collect();
    let times: Vec<_> = initial.iter().map(|r| r.admitted_at).collect();
    // Aggregate in PostgreSQL after locking the bounded whole-job selection;
    // the gateway receives one evidence row per job, never every line attempt.
    let lines=sqlx::query_as("WITH locked AS MATERIALIZED (SELECT e.batch_job_id,r.state,r.actual_microusd,r.held_microusd,r.unbounded_cost FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at WHERE (r.execution_id,r.admitted_at) IN (SELECT * FROM unnest($1::uuid[],$2::timestamptz[])) OR e.batch_job_id=ANY($3) ORDER BY r.admitted_at,r.execution_id FOR UPDATE OF r) SELECT batch_job_id,coalesce(sum(actual_microusd) FILTER(WHERE state='settled'),0)::bigint AS settled,coalesce(sum(held_microusd) FILTER(WHERE state<>'settled'),0)::bigint AS held,count(*) FILTER(WHERE state='pending') AS pending,count(*) FILTER(WHERE state='unknown') AS unknown,count(*) FILTER(WHERE state<>'settled' AND (unbounded_cost OR held_microusd IS NULL)) AS unbounded FROM locked WHERE batch_job_id IS NOT NULL GROUP BY batch_job_id")
        .bind(&executions).bind(&times).bind(&jobs).fetch_all(&mut **tx).await?;
    // Job locks exclude line admission. Read missing line reservations once
    // for the page, rather than a correlated anti-join for each envelope.
    let ambiguous:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT e.batch_job_id FROM inference_executions e JOIN async_jobs j ON j.id=e.batch_job_id WHERE e.batch_job_id IN (SELECT unnest($1::uuid[])) AND (e.workspace_id<>j.workspace_id OR NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id AND r.admitted_at=e.started_at))")
        .bind(&jobs).fetch_all(&mut **tx).await?;
    Ok((lines, ambiguous.into_iter().collect()))
}
fn evidence(rows: &[Candidate], lines: &[Line]) -> BTreeMap<Uuid, Evidence> {
    let mut result = BTreeMap::new();
    for r in rows {
        result.insert(
            r.job_id,
            Evidence {
                envelope_held: r.held_microusd,
                ..Default::default()
            },
        );
    }
    for line in lines {
        let Some(e) = result.get_mut(&line.batch_job_id) else {
            continue;
        };
        e.settled = line.settled;
        e.held = line.held;
        e.pending = line.pending;
        e.unknown = line.unknown;
        e.unbounded = line.unbounded;
    }
    result
}
pub(super) async fn resolve(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Json(p): Json<Resolve>,
) -> ApiResult {
    p.validate()?;
    if u.token_id().is_some() {
        return Err(denied());
    }
    tokio::time::timeout(std::time::Duration::from_secs(60), resolve_page(s, u, p))
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Resolution timed out; preview again",
            )
        })?
}
async fn resolve_page(s: Store, u: ManagementPrincipal, p: Resolve) -> ApiResult {
    let offset = p.offset()?;
    let mut tx = crate::db::begin(&s.pool).await?;
    sqlx::query(
        "SELECT set_config('statement_timeout','10s',true),set_config('lock_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    resources::catalog_lock(&mut tx, false).await?;
    resources::require_platform_admin(
        &resources::platform_role_snapshot(&mut tx, u.user_id()).await?,
    )?;
    let mut initial = candidates(&mut tx, &p, None).await?;
    let more = initial.len() > MAX_CANDIDATES;
    initial.truncate(MAX_CANDIDATES);
    locks::shared(
        &mut tx,
        initial
            .iter()
            .map(|r| Scope::Workspace(r.workspace_id))
            .chain([Scope::User(u.user_id())]),
    )
    .await?;
    // The actor scope is held through all later job/reservation waits.
    resources::platform_write(&mut tx, &u).await?;
    let (scanned, more) = paging::bound(&mut tx, &mut initial, more).await?;
    let (lines, ambiguous) = locked(&mut tx, &initial).await?;
    let mut rows = candidates(&mut tx, &p, Some(&initial)).await?;
    rows.retain(|r| !ambiguous.contains(&r.job_id));
    let count = rows.len().to_string();
    let evidence = evidence(&rows, &lines);
    let mut total = Evidence::default();
    for e in evidence.values() {
        total.include(e)?;
    }
    if p.apply {
        if p.expected_count.as_deref() != Some(count.as_str()) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Eligible count changed; preview again",
            ));
        }
        let touches: Vec<_> = rows
            .iter()
            .map(|r| locks::Touch {
                workspace: r.workspace_id,
                api_key: r.api_key_id,
                at: r.admitted_at,
            })
            .collect();
        locks::rows(&mut tx, &touches, false).await?;
        apply(
            &mut tx,
            &rows,
            &evidence,
            u.user_id(),
            p.reason.as_deref().ok_or_else(invalid)?,
        )
        .await?;
    }
    let next =
        more.then(|| (offset + scanned - if p.apply { rows.len() as i64 } else { 0 }).to_string());
    let mut result = total.json();
    result["eligible_count"] = json!(count);
    result["resolved_count"] = json!(if p.apply {
        rows.len().to_string()
    } else {
        "0".into()
    });
    result["next_offset"] = json!(next);
    // No retry on COMMIT errors: the transaction may already be durable.
    commit(tx).await?;
    Ok(Json(result))
}
// Bound the transition tables consumed by the existing totals triggers. All
// page locks are already held, and every group shares one transaction.
const MAX_UPDATE_ROWS: usize = 512;
async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[Candidate],
    evidence: &BTreeMap<Uuid, Evidence>,
    actor: Uuid,
    reason: &str,
) -> Result<(), ApiError> {
    let executions: Vec<_> = rows.iter().map(|r| r.execution_id).collect();
    let times: Vec<_> = rows.iter().map(|r| r.admitted_at).collect();
    let jobs: Vec<_> = rows.iter().map(|r| r.job_id).collect();
    let settled: Vec<_> = rows.iter().map(|r| evidence[&r.job_id].settled).collect();
    for (ids, ats) in executions
        .chunks(MAX_UPDATE_ROWS)
        .zip(times.chunks(MAX_UPDATE_ROWS))
    {
        let changed=sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=0,batch_envelope_resolved=true WHERE (execution_id,admitted_at) IN (SELECT * FROM unnest($1::uuid[],$2::timestamptz[])) AND state='unknown'")
            .bind(ids).bind(ats).execute(&mut **tx).await?;
        if changed.rows_affected() != ids.len() as u64 {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Eligible count changed; preview again",
            ));
        }
    }
    // Copy observations byte-for-byte, including NULLs; known line costs are
    // evidence only, because their reservations already charged them.
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components,meter_usage,output_image_variant,provider_cost_microusd,evidence) SELECT gen_random_uuid(),r.execution_id,r.admitted_at,'reconciliation',0,r.input_tokens,r.output_tokens,r.billing_usage,r.cost_components,r.meter_usage,r.output_image_variant,r.provider_cost_microusd,'batch-lines='||s.settled::text||';'||$4::text FROM governance_reservations r JOIN unnest($1::uuid[],$2::timestamptz[],$3::bigint[]) s(execution_id,admitted_at,settled) ON s.execution_id=r.execution_id AND s.admitted_at=r.admitted_at")
        .bind(&executions).bind(&times).bind(&settled).bind(reason).execute(&mut **tx).await?;
    sqlx::query("UPDATE async_jobs SET settled_at=clock_timestamp() WHERE id=ANY($1) AND settled_at IS NULL")
        .bind(&jobs).execute(&mut **tx).await?;
    let mut workspaces = BTreeMap::<Uuid, (i64, Evidence)>::new();
    for r in rows {
        let e = &evidence[&r.job_id];
        let (count, total) = workspaces.entry(r.workspace_id).or_default();
        *count += 1;
        total.include(e)?;
    }
    for (workspace, (count, total)) in workspaces {
        let mut metadata = total.json();
        metadata["reason"] = json!(reason);
        metadata["resolved_count"] = json!(count.to_string());
        // The actor's export can include this event: every resource/metadata
        // field must remain aggregate-only even for another owner's workspace.
        sqlx::query("INSERT INTO audit_events(id,workspace_id,actor_user_id,action,resource_type,resource_id,metadata) VALUES(gen_random_uuid(),$1,$2,'usage.batch_envelope_resolved','workspace',$1,$3)").bind(workspace).bind(actor).bind(metadata).execute(&mut **tx).await?;
    }
    Ok(())
}
#[cfg(test)]
tokio::task_local! {pub(crate) static LOSE_COMMIT_ACK: bool;}
async fn commit(tx: Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    tx.commit().await?;
    #[cfg(test)]
    if LOSE_COMMIT_ACK.try_with(|lose| *lose).unwrap_or(false) {
        return Err(sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "commit acknowledgement lost",
        )));
    }
    Ok(())
}
#[test]
fn management_openapi_batch_envelope_request_vectors() {
    crate::management::openapi::requests::vectors::<Resolve>(
        "POST",
        "/api/v1/platform/reservations/resolve-batch-envelopes",
        json!({"apply":true,"month":"2026-01","workspace_id":Uuid::nil(),"deployment_id":Uuid::nil(),"expected_count":"0","reason":"operator reference","offset":"0"}),
    );
}
