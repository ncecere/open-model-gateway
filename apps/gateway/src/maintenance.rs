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
    compact_history_fenced(store, days, limit, None)
        .await
        .map_err(|_| InferenceError::Storage)?
}
/// [`compact_history`] in a transaction fenced to a `compaction` lease term
/// (`None`: the explicit operator command).
pub async fn compact_history_fenced(
    store: &Store,
    days: i32,
    limit: i64,
    fence: Option<&crate::leases::Fence>,
) -> Result<Result<u64, InferenceError>, sqlx::Error> {
    if !(30..=3650).contains(&days) || !(1..=1000).contains(&limit) {
        return Ok(Err(InferenceError::InvalidRequest));
    }
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let changed=sqlx::query("WITH old AS (SELECT e.id FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.completed_at < now()-make_interval(days=>$1) AND e.details_redacted_at IS NULL AND r.state='settled' ORDER BY e.completed_at,e.id LIMIT $2 FOR UPDATE OF e SKIP LOCKED) UPDATE inference_executions e SET error_code=NULL,elapsed_ms=NULL,client_session_id=NULL,client_app=NULL,details_redacted_at=clock_timestamp() FROM old WHERE e.id=old.id")
        .bind(days).bind(limit).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(Ok(changed))
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
/// Settings are re-read at least this often even without a change notice.
const SETTINGS_REFRESH: Duration = Duration::from_secs(60);
/// Every replica, every 5 s: installation settings (re-read when the
/// `settings` version changes, at least every minute, and every tick while
/// the versions are unconfirmed) and expired-lease reconciliation (a
/// `SKIP LOCKED` queue: replicas never process or wait on the same rows).
/// Every replica, every 5 s: its recorded scope-lock waits (0036).
/// Lease holders only (`crate::leases`; every replica without leases, e.g.
/// tests): rate-counter and lock-wait pruning every minute and storage-usage hours every 5
/// minutes (`maintenance`), detail compaction hourly (`compaction`).
/// `retention_days` is the environment override; without it the
/// installation setting applies. Compaction never touches the ledger,
/// reservations, prices or audit history.
pub fn start(store: Store, retention_days: Option<i32>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut counter = 0u64;
        let mut installation_retention = None;
        let mut settings_seen: Option<(u64, std::time::Instant)> = None;
        loop {
            tick.tick().await;
            let versions = &store.caches.versions;
            let settings_version = versions.version(crate::notify::Topic::Settings);
            let due = match settings_seen {
                Some((seen, at)) => {
                    !versions.fresh()
                        || seen != settings_version
                        || at.elapsed() >= SETTINGS_REFRESH
                }
                None => true,
            };
            if due {
                match tokio::time::timeout(Duration::from_secs(2), refresh_runtime_settings(&store))
                    .await
                {
                    Ok(Ok(days)) => {
                        installation_retention = days;
                        settings_seen = Some((settings_version, std::time::Instant::now()));
                    }
                    _ => {
                        tracing::warn!("installation settings refresh failed; keeping last values")
                    }
                }
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
            // This replica's scope-lock waits (0036), every tick: the
            // `admission_ceiling` alert reads them from the database.
            match tokio::time::timeout(
                Duration::from_secs(2),
                crate::governance::pressure::flush(&store),
            )
            .await
            {
                Ok(Ok(_)) => {}
                _ => crate::metrics::METRICS.observe_collection_error("scope_lock_waits"),
            }
            // Minute rate counters admission no longer reads (0024), every minute.
            if counter.is_multiple_of(12) {
                singleton(
                    &store,
                    crate::leases::Lease::Maintenance,
                    "lock_wait_prune",
                    |fence| {
                        let store = store.clone();
                        async move {
                            crate::governance::pressure::prune_fenced(
                                &store,
                                10_000,
                                fence.as_ref(),
                            )
                            .await
                        }
                    },
                )
                .await;
                singleton(
                    &store,
                    crate::leases::Lease::Maintenance,
                    "rate_prune",
                    |fence| {
                        let store = store.clone();
                        async move {
                            crate::governance::rates::prune_fenced(&store, 10_000, fence.as_ref())
                                .await
                        }
                    },
                )
                .await;
            }
            // Storage usage (not charged): record completed hours every 5 minutes.
            if counter.is_multiple_of(60) {
                singleton(
                    &store,
                    crate::leases::Lease::Maintenance,
                    "storage_usage",
                    |fence| {
                        let store = store.clone();
                        async move {
                            crate::filestore::usage::record_hours_fenced(&store, 48, fence.as_ref())
                                .await
                        }
                    },
                )
                .await;
            }
            if counter.is_multiple_of(720)
                && let Some(days) = retention_days.or(installation_retention)
            {
                let compacted =
                    singleton(
                        &store,
                        crate::leases::Lease::Compaction,
                        "compaction",
                        |fence| {
                            let store = store.clone();
                            async move {
                                compact_history_fenced(&store, days, 1000, fence.as_ref()).await
                            }
                        },
                    )
                    .await;
                if let Some(Ok(n)) = compacted
                    && n > 0
                {
                    tracing::info!(
                        executions = n,
                        "settled execution details compacted; monetary history retained"
                    );
                }
            }
            counter = counter.wrapping_add(1);
        }
    })
}

/// Run `work` once: under `lease` when this replica runs leased background
/// work (serve), skipping the tick unless it holds the lease; otherwise
/// (no leases configured) unconditionally, as before P5.
pub(crate) async fn singleton<T, F, Fut>(
    store: &Store,
    lease: crate::leases::Lease,
    job: &'static str,
    work: F,
) -> Option<T>
where
    F: FnOnce(Option<crate::leases::Fence>) -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let timeout = Duration::from_secs(4);
    match store.leases() {
        Some(leases) => {
            crate::leases::run_singleton(leases, lease, job, timeout, |fence| work(Some(fence)))
                .await
        }
        None => match tokio::time::timeout(timeout, work(None)).await {
            Ok(Ok(value)) => Some(value),
            _ => {
                tracing::warn!(job, "background job run incomplete; retrying next interval");
                None
            }
        },
    }
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
