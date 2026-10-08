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
    let changed=sqlx::query("WITH old AS (SELECT e.id FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.completed_at < now()-make_interval(days=>$1) AND e.details_redacted_at IS NULL AND r.state='settled' ORDER BY e.completed_at,e.id LIMIT $2 FOR UPDATE OF e SKIP LOCKED) UPDATE inference_executions e SET error_code=NULL,elapsed_ms=NULL,client_session_id=NULL,client_app=NULL,details_redacted_at=clock_timestamp() FROM old WHERE e.id=old.id")
        .bind(days).bind(limit).execute(&store.pool).await.map_err(|_|InferenceError::Storage)?.rows_affected();
    Ok(changed)
}
/// Installation settings (0010) that runtime work follows: the OpenRouter data
/// collection default (applied immediately) and the request log retention.
/// Environment overrides win where set.
pub async fn refresh_runtime_settings(store: &Store) -> Result<Option<i32>, InferenceError> {
    let (policy, retention): (String, Option<i32>) = sqlx::query_as(
        "SELECT openrouter_data_collection,request_log_retention_days FROM installation_settings WHERE singleton",
    )
    .fetch_one(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?;
    crate::providers::openrouter::DataCollection::set_installation_default(
        crate::providers::openrouter::DataCollection::parse(&policy).unwrap_or_default(),
    );
    Ok(retention)
}
/// `retention_days` is the environment override; without it the installation
/// setting (refreshed every tick) applies. Compaction never touches the ledger,
/// reservations, prices or audit history.
pub fn start(store: Store, retention_days: Option<i32>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut counter = 0u64;
        let mut installation_retention = None;
        loop {
            tick.tick().await;
            match tokio::time::timeout(Duration::from_secs(2), refresh_runtime_settings(&store))
                .await
            {
                Ok(Ok(days)) => installation_retention = days,
                _ => tracing::warn!("installation settings refresh failed; keeping last values"),
            }
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
                && let Some(days) = retention_days.or(installation_retention)
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
