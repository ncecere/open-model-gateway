//! Reconciliation: expired leases become unknown, and evidence-backed usage
//! resolves an unknown reservation.
use super::*;
/// Expired-lease reservations reconciled per transaction.
const RECONCILE_BATCH: i64 = 50;
/// Marks up to `limit` expired pending reservations unknown (their holds are
/// retained) and their executions cancelled. Work is claimed in batches of
/// [`RECONCILE_BATCH`] with `FOR UPDATE SKIP LOCKED`, one transaction per
/// batch, so concurrent replicas never process (or wait on) the same rows.
/// Scoped: claimed reservation rows, then their totals rows in canonical
/// order; global: the installation lock once per batch.
pub async fn reconcile_expired(store: &Store, limit: i64) -> Result<u64, InferenceError> {
    if !(1..=10_000).contains(&limit) {
        return Err(InferenceError::InvalidRequest);
    }
    let mut count = 0u64;
    while (count as i64) < limit {
        let batch = RECONCILE_BATCH.min(limit - count as i64);
        let _queued = gate(&store.lock_gates.settlement).await;
        let ids = locks::retry_deadlocks!("reconciliation", reconcile_batch(store, batch).await)?;
        count += ids as u64;
        if (ids as i64) < batch {
            break;
        }
    }
    Ok(count)
}
async fn reconcile_batch(store: &Store, batch: i64) -> Result<usize, InferenceError> {
    {
        let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
        settlement_prefix(store, &mut tx).await?;
        let due: Vec<(Uuid, Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as("SELECT execution_id,workspace_id,api_key_id,admitted_at FROM governance_reservations WHERE state='pending' AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at,execution_id LIMIT $1 FOR UPDATE SKIP LOCKED")
            .bind(batch)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage)?;
        let touches: Vec<locks::Touch> = due
            .iter()
            .map(|d| locks::Touch {
                workspace: d.1,
                api_key: d.2,
                at: d.3,
            })
            .collect();
        lock_rows(store, &mut tx, &touches, false).await?;
        let claimed: Vec<Uuid> = due.iter().map(|d| d.0).collect();
        let ids: Vec<Uuid> = sqlx::query_scalar("UPDATE governance_reservations SET state='unknown' WHERE execution_id=ANY($1) AND state='pending' RETURNING execution_id")
            .bind(&claimed)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage)?;
        if ids.len() != claimed.len() {
            return Err(InferenceError::Storage);
        }
        if !ids.is_empty() {
            let changed=sqlx::query("UPDATE inference_executions SET state='cancelled',error_code='lease_expired',finish_reason='cancelled',completed_at=clock_timestamp() WHERE id=ANY($1) AND state='started'").bind(&ids).execute(&mut *tx).await.map_err(storage)?.rows_affected();
            if changed != ids.len() as u64 {
                return Err(InferenceError::Storage);
            }
            // The ledger row `ledger()` writes for an unknown outcome without
            // usage: no amount, no observations.
            sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,admitted_at) SELECT gen_random_uuid(),r.execution_id,'unknown',r.admitted_at FROM governance_reservations r WHERE r.execution_id=ANY($1::uuid[])")
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
        Ok(ids.len())
    }
}
/// The identity performing a usage reconciliation: the owning user, and --
/// when an admin token (not a browser session) authenticated the call --
/// that token's id (#37 PR2 review: the blocking finding that
/// `resolve_usage`'s caller authenticates in its own, already-committed
/// transaction before ever reaching this one, so an explicit revoke or
/// expiry between the two must still be caught here, inside the write's own
/// transaction, not just at the earlier authentication).
///
/// `From<Uuid>` (a plain browser/test actor, `token_id: None`) keeps every
/// existing caller of `resolve_usage` compiling unchanged: none of them
/// need to change for this fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcilingActor {
    pub user_id: Uuid,
    pub token_id: Option<Uuid>,
}
impl From<Uuid> for ReconcilingActor {
    fn from(user_id: Uuid) -> Self {
        Self {
            user_id,
            token_id: None,
        }
    }
}
/// The token-row liveness check (#37 PR2 review): unrevoked, unexpired and
/// still owned by `actor.user_id`, `FOR SHARE` so a concurrent explicit
/// revoke blocks on this transaction until it commits. A no-op for a
/// browser actor (`token_id: None`). Must run after this transaction's own
/// ordered authority locks (so a concurrent owner-loss trigger cannot race
/// ahead of it) and before the reservation row or any write.
async fn revalidate_token(tx: &mut Tx<'_>, actor: ReconcilingActor) -> Result<(), InferenceError> {
    let Some(token_id) = actor.token_id else {
        return Ok(());
    };
    if !crate::admin_tokens::token_row_live(tx, token_id, actor.user_id)
        .await
        .map_err(storage)?
    {
        return Err(InferenceError::Unauthenticated);
    }
    Ok(())
}
/// Authorization remains live after waiting. Personal details never become platform-public.
pub async fn resolve_usage(
    store: &Store,
    workspace: Uuid,
    execution: Uuid,
    usage: Usage,
    evidence: &str,
    actor: impl Into<ReconcilingActor>,
) -> Result<(), InferenceError> {
    let actor = actor.into();
    if evidence.trim().is_empty()
        || evidence.chars().count() > 200
        || evidence.chars().any(char::is_control)
    {
        return Err(InferenceError::InvalidRequest);
    }
    locks::retry_deadlocks!(
        "reconciliation",
        resolve_usage_once(store, workspace, execution, usage, evidence, actor).await
    )
}
#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
async fn resolve_usage_once(
    store: &Store,
    workspace: Uuid,
    execution: Uuid,
    usage: Usage,
    evidence: &str,
    actor: ReconcilingActor,
) -> Result<(), InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
    if scoped(store) {
        // The actor's authority (workspace, user) cannot be revoked until
        // commit; then the reservation row, then its totals rows.
        catalog_lock(&mut tx).await?;
        locks::shared(
            &mut tx,
            [
                locks::Scope::Workspace(workspace),
                locks::Scope::User(actor.user_id),
            ],
        )
        .await
        .map_err(storage)?;
    } else {
        lock(&mut tx).await?;
        // Global mode takes no User-scope advisory lock for a browser actor
        // (preserving this mode's original behaviour exactly); a token
        // actor needs the owner's authority lock held before the token
        // row check below, or a concurrent owner-loss trigger could revoke
        // the token between this check and the role check that follows,
        // observing neither consistently (#37 PR2 review).
        if actor.token_id.is_some() {
            locks::shared(&mut tx, [locks::Scope::User(actor.user_id)])
                .await
                .map_err(storage)?;
        }
    }
    revalidate_token(&mut tx, actor).await?;
    let r = reservation(&mut tx, execution, scoped(store)).await?;
    // Realtime sessions are valued per response (`realtime`); an aggregate
    // reconciliation could not reproduce that valuation, so it is refused.
    if r.workspace_id != workspace || r.workload_kind == WorkloadKind::Realtime.as_str() {
        return Err(InferenceError::InvalidRequest);
    }
    let authorized:Option<Uuid>=sqlx::query_scalar(&format!("SELECT u.id FROM users u JOIN effective_platform_roles p ON p.user_id=u.id AND p.role='admin' JOIN workspaces w ON w.id=$2 AND w.disabled_at IS NULL WHERE u.id=$1 AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND (w.kind IN('team','project') OR w.owner_user_id=u.id){}", if scoped(store) { "" } else { " FOR SHARE OF u,w" }))
        .bind(actor.user_id).bind(workspace).fetch_optional(&mut *tx).await.map_err(storage)?;
    if authorized.is_none() {
        return Err(InferenceError::InvalidRequest);
    }
    let usage = workload_usage(&r, usage, InferenceError::InvalidRequest)?;
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
    lock_rows(store, &mut tx, &[r.touch()], false).await?;
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=$2,input_tokens=$3,output_tokens=$4,billing_usage=$5,cost_components=$6,meter_usage=$7,output_image_variant=$8,provider_cost_microusd=$9 WHERE execution_id=$1 AND admitted_at=$10").bind(execution).bind(actual).bind(input).bind(output).bind(&billing).bind(value.components.map(|c|c.to_value())).bind(&m.meters).bind(&m.variant).bind(m.provider_cost).bind(r.admitted_at).execute(&mut *tx).await.map_err(storage)?;
    sqlx::query("UPDATE inference_executions SET input_tokens=$2,output_tokens=$3,billing_usage=$4,meter_usage=$5,output_image_variant=$6,provider_cost_microusd=$7 WHERE id=$1 AND started_at=$8").bind(execution).bind(input).bind(output).bind(billing).bind(m.meters).bind(m.variant).bind(m.provider_cost).bind(r.admitted_at).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        execution,
        Some(r.admitted_at),
        "reconciliation",
        Some(actual),
        usage,
        value.components,
        Some(evidence),
    )
    .await?;
    sqlx::query("INSERT INTO audit_events(id,workspace_id,actor_user_id,action,resource_type,resource_id,metadata,admin_token_id) VALUES($1,$2,$3,'usage.reconciled','execution',$4,'{}',$5)").bind(Uuid::new_v4()).bind(workspace).bind(actor.user_id).bind(execution).bind(actor.token_id).execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)
}
