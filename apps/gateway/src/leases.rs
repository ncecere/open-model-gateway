//! Work leases for singleton background jobs (migration 0029, scale plan P5).
//!
//! Every replica runs one [`Leases`] task that takes or renews each lease row
//! every [`RENEW`] (term [`TTL`]). A job runs only on the replica whose term
//! is current, and every job transaction starts with [`Fence::check`]
//! (`omg_lease_fence`): it fails unless the term is still current and
//! unexpired in the database, and holds the lease row `FOR SHARE` until
//! commit, so a takeover waits for a running job transaction and a paused
//! former leader can never commit work after its term ended. A replica that
//! dies stops renewing; another takes over once the term expires (at most
//! `TTL + RENEW` later). Graceful shutdown releases the terms at once.
//!
//! Locally a term is trusted until `TTL - MARGIN` after the renewal request
//! was sent, which is never later than the database expiry.
//!
//! Queues (expired-reservation reconciliation, async-job polling, gateway-run
//! batch lines, alert deliveries) are not leased: every replica claims work
//! with `FOR UPDATE SKIP LOCKED` (or per-batch runner leases).
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Term length in the database.
pub const TTL: Duration = Duration::from_secs(30);
/// Renewal interval.
pub const RENEW: Duration = Duration::from_secs(10);
/// A term is trusted locally until this long before its nominal end.
const MARGIN: Duration = Duration::from_secs(3);

/// The seeded leases (`work_leases.name`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lease {
    /// Alert rule evaluation (deliveries are a `SKIP LOCKED` queue).
    Alerts,
    /// Settled execution detail compaction (hourly).
    Compaction,
    /// Expired/abandoned stored-file sweeper (every minute).
    FileSweep,
    /// Account lifecycle cleanup (every minute).
    Lifecycle,
    /// Rate-counter pruning (every minute) and storage-usage hours (5 min).
    Maintenance,
    /// The installation-wide `gateway_reservations_held` gauge.
    Metrics,
}

impl Lease {
    pub const ALL: [Self; 6] = [
        Self::Alerts,
        Self::Compaction,
        Self::FileSweep,
        Self::Lifecycle,
        Self::Maintenance,
        Self::Metrics,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alerts => "alerts",
            Self::Compaction => "compaction",
            Self::FileSweep => "file_sweep",
            Self::Lifecycle => "lifecycle",
            Self::Maintenance => "maintenance",
            Self::Metrics => "metrics",
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}

/// One current term: the fencing token of every job transaction it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub lease: Lease,
    pub holder: Uuid,
    pub epoch: i64,
}

impl Fence {
    /// First statement of a job transaction: fail unless this term is still
    /// current, and hold the lease row (`FOR SHARE`) until commit.
    pub async fn check(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT omg_lease_fence($1,$2,$3)")
            .bind(self.lease.as_str())
            .bind(self.holder)
            .bind(self.epoch)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Record a completed run (fenced); whether this term recorded it.
    pub async fn complete(&self, pool: &PgPool) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT omg_lease_complete($1,$2,$3)")
            .bind(self.lease.as_str())
            .bind(self.holder)
            .bind(self.epoch)
            .fetch_one(pool)
            .await
    }
}

/// Fence `tx` when running under a lease (`None`: an explicit operator
/// command, which is not leased).
pub async fn fence(
    tx: &mut Transaction<'_, Postgres>,
    fence: Option<&Fence>,
) -> Result<(), sqlx::Error> {
    match fence {
        Some(f) => f.check(tx).await,
        None => Ok(()),
    }
}

/// Whether `error` is a fencing failure (the term ended; SQLSTATE 55006).
pub fn is_fenced(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().as_deref() == Some("55006"))
}

#[derive(Clone, Copy)]
struct Term {
    epoch: i64,
    trusted_until: Instant,
}

/// This replica's lease terms.
pub struct Leases {
    holder: Uuid,
    ttl: Duration,
    margin: Duration,
    terms: [Mutex<Option<Term>>; 6],
}

impl Default for Leases {
    fn default() -> Self {
        Self::new()
    }
}

impl Leases {
    pub fn new() -> Self {
        Self::with_ttl(TTL)
    }

    /// Custom term length (tests); the margin scales with it.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            holder: Uuid::new_v4(),
            ttl: ttl.max(Duration::from_secs(1)),
            margin: MARGIN.min(ttl / 10),
            terms: Default::default(),
        }
    }

    /// This replica's holder id.
    pub fn holder(&self) -> Uuid {
        self.holder
    }

    /// The current term of `lease`, if this replica holds it.
    pub fn held(&self, lease: Lease) -> Option<Fence> {
        let term = (*self.terms[lease.index()]
            .lock()
            .unwrap_or_else(|e| e.into_inner()))?;
        (term.trusted_until > Instant::now()).then_some(Fence {
            lease,
            holder: self.holder,
            epoch: term.epoch,
        })
    }

    /// Take or renew every lease once. Returns how many this replica holds.
    pub async fn renew_once(&self, pool: &PgPool) -> usize {
        let mut held = 0;
        for lease in Lease::ALL {
            let sent = Instant::now();
            let acquired: Result<Option<i64>, sqlx::Error> = tokio::time::timeout(
                Duration::from_secs(5),
                sqlx::query_scalar("SELECT omg_lease_acquire($1,$2,$3)")
                    .bind(lease.as_str())
                    .bind(self.holder)
                    .bind(self.ttl.as_secs().clamp(1, 3600) as i32)
                    .fetch_one(pool),
            )
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut));
            let mut slot = self.terms[lease.index()]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match acquired {
                Ok(Some(epoch)) => {
                    if slot.is_none_or(|t| t.epoch != epoch) {
                        crate::metrics::METRICS.observe_lease_term(lease.as_str());
                        tracing::info!(lease = lease.as_str(), epoch, "work lease term started");
                    }
                    *slot = Some(Term {
                        epoch,
                        trusted_until: sent + self.ttl - self.margin,
                    });
                    held += 1;
                }
                // Another replica's term is current.
                Ok(None) => *slot = None,
                // Unknown: keep trusting the term until its local deadline.
                Err(_) => {
                    if slot.is_some_and(|t| t.trusted_until > Instant::now()) {
                        held += 1;
                    }
                }
            }
            crate::metrics::METRICS.set_lease_held(
                lease.as_str(),
                slot.is_some_and(|t| t.trusted_until > Instant::now()),
            );
        }
        held
    }

    /// End every term this replica holds (graceful shutdown).
    pub async fn release_all(&self, pool: &PgPool) {
        for lease in Lease::ALL {
            let term = self.terms[lease.index()]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(term) = term {
                let _ = sqlx::query("SELECT omg_lease_release($1,$2,$3)")
                    .bind(lease.as_str())
                    .bind(self.holder)
                    .bind(term.epoch)
                    .execute(pool)
                    .await;
                crate::metrics::METRICS.set_lease_held(lease.as_str(), false);
            }
        }
    }

    /// Renew every `every` until aborted.
    pub fn start(self: Arc<Self>, pool: PgPool, every: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.renew_once(&pool).await;
            }
        })
    }
}

/// Run one tick of a singleton job if this replica holds `lease`: `job`
/// receives the fence for its transactions. Fencing failures (the term ended
/// mid-run) are expected and reported as `fenced`.
pub async fn run_singleton<T, F, Fut>(
    leases: &Leases,
    lease: Lease,
    job: &'static str,
    timeout: Duration,
    work: F,
) -> Option<T>
where
    F: FnOnce(Fence) -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let fence = leases.held(lease)?;
    match tokio::time::timeout(timeout, work(fence)).await {
        Ok(Ok(value)) => {
            crate::metrics::METRICS.observe_background_run(job, "ok");
            Some(value)
        }
        Ok(Err(error)) if is_fenced(&error) => {
            crate::metrics::METRICS.observe_background_run(job, "fenced");
            tracing::info!(job, "work lease term ended during a run; nothing committed");
            None
        }
        _ => {
            crate::metrics::METRICS.observe_background_run(job, "failed");
            tracing::warn!(job, "background job run incomplete; retrying next interval");
            None
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "leases/tests.rs"]
mod tests;
