//! Governance touch points of async jobs (`crate::jobs`). Admission and
//! settlement of a job attempt go through the ordinary `admit_*`/`finish`
//! functions, so budget totals (0015 triggers) stay correct; this module only
//! adds the lease extension of an admitted job and a read-only bound preview.
use super::*;

/// Extend the lease of a still-pending job reservation to `until` (never
/// shortens). Serialized on the installation lock like every other change
/// to live leases (they count toward concurrency). Returns whether the
/// reservation was still pending.
pub async fn extend_lease(
    store: &Store,
    execution: Uuid,
    until: DateTime<Utc>,
) -> Result<bool, InferenceError> {
    let _queued = gate(&store.lock_gates.settlement).await;
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    lock(&mut tx).await?;
    let changed = sqlx::query("UPDATE governance_reservations SET lease_expires_at=greatest(lease_expires_at,$2) WHERE execution_id=$1 AND state='pending'")
        .bind(execution)
        .bind(until)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
    tx.commit().await.map_err(storage)?;
    Ok(changed == 1)
}

/// Preview of the conservative hold a batch of `requests` lines with these
/// output maxima would take under the deployment's current price, without
/// reserving anything. `Ok(None)` when unpriced, out of the price's
/// ceilings or unbounded (admission would refuse it under any budget).
pub async fn batch_bound_preview(
    store: &Store,
    deployment: Uuid,
    requests: u32,
    output_tokens: u64,
    max_line_output: u32,
) -> Result<Option<i64>, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(storage)?;
    let price = sqlx::query_as::<_, Price>(&format!("SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1"))
        .bind(deployment)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
    tx.commit().await.map_err(storage)?;
    let Some(p) = price else {
        return Ok(None);
    };
    if requests == 0
        || max_line_output == 0
        || p.input_token_limit < 1
        || i64::from(max_line_output) > p.output_token_limit
    {
        return Ok(None);
    }
    let ceilings = MeterUsage {
        requests: Some(u64::from(requests)),
        ..MeterUsage::default()
    };
    match p.bound_scaled(u64::from(requests), output_tokens, &ceilings, false) {
        Ok(bound) => Ok(bound),
        Err(InferenceError::Configuration) => Ok(None),
        Err(e) => Err(e),
    }
}
