//! Bounded, replica-safe accounting maintenance. No unknown-cost refunds.
use crate::{inference::error::InferenceError, store::Store};
use std::time::Duration;

pub fn retention_from_env() -> anyhow::Result<Option<i32>> {
    match std::env::var("GATEWAY_EXECUTION_DETAIL_RETENTION_DAYS") {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Ok(value) => {
            let days: i32 = value
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid execution detail retention"))?;
            anyhow::ensure!(
                (30..=3650).contains(&days),
                "execution detail retention must be 30..3650 days"
            );
            Ok(Some(days))
        }
        Err(_) => anyhow::bail!("invalid execution detail retention"),
    }
}
pub async fn compact_history(store: &Store, days: i32, limit: i64) -> Result<u64, InferenceError> {
    if !(30..=3650).contains(&days) || !(1..=1000).contains(&limit) {
        return Err(InferenceError::InvalidRequest);
    }
    let changed=sqlx::query("WITH old AS (SELECT e.id FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.completed_at < now()-make_interval(days=>$1) AND e.details_redacted_at IS NULL AND r.state='settled' ORDER BY e.completed_at,e.id LIMIT $2 FOR UPDATE OF e SKIP LOCKED) UPDATE inference_executions e SET error_code=NULL,elapsed_ms=NULL,details_redacted_at=clock_timestamp() FROM old WHERE e.id=old.id")
        .bind(days).bind(limit).execute(&store.pool).await.map_err(|_|InferenceError::Storage)?.rows_affected();
    Ok(changed)
}
pub fn start(store: Store, retention_days: Option<i32>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut counter = 0u64;
        loop {
            tick.tick().await;
            match tokio::time::timeout(
                Duration::from_secs(4),
                crate::governance::reconcile_expired(&store, 100),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => tracing::info!(
                    executions = n,
                    "expired execution leases reconciled; unknown cost retained"
                ),
                Ok(Ok(_)) => {}
                _ => tracing::warn!("execution reconciliation incomplete; retrying next interval"),
            }
            if counter.is_multiple_of(720)
                && let Some(days) = retention_days
            {
                match tokio::time::timeout(
                    Duration::from_secs(4),
                    compact_history(&store, days, 1000),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => tracing::info!(
                        executions = n,
                        "settled execution details compacted; monetary history retained"
                    ),
                    Ok(Ok(_)) => {}
                    _ => tracing::warn!("execution detail compaction incomplete"),
                }
            }
            counter = counter.wrapping_add(1);
        }
    })
}
#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn bounds_reject_unsafe_retention(pool: sqlx::PgPool) {
        let store = Store::new(pool);
        assert!(compact_history(&store, 0, 100).await.is_err());
        assert!(compact_history(&store, 30, 1001).await.is_err());
        assert_eq!(compact_history(&store, 30, 100).await.unwrap(), 0);
    }
}
