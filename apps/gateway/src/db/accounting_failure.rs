//! Production signal for the cause of `503 accounting_unavailable` (#389).
//!
//! Each failed accounting operation increments
//! `gateway_accounting_failures_total{stage,cause}` once and writes one WARN
//! line carrying the same two fields. Both values come only from the closed
//! enums below: never SQL text, bind values, a raw SQLSTATE, a driver or
//! database message, an identifier or a credential. Nothing here runs when
//! accounting succeeds.
use std::{borrow::Cow, cell::Cell, future::Future};

/// Where the accounting operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Starting an accounting transaction: taking a pooled connection and
    /// `BEGIN` (sqlx does not separate the two). Also an accounting statement
    /// run without a transaction that got no connection (a pool timeout or a
    /// closed pool).
    PoolAcquireAndBegin,
    /// Any later statement of an accounting transaction, including `COMMIT`,
    /// and a statement run directly on the pool once it has a connection.
    /// Known limit: an error raised while opening a new connection for such a
    /// statement, other than a pool timeout or a closed pool (an
    /// authentication, TLS or I/O failure during connect), is counted here
    /// although no statement ran.
    GovernanceStorage,
    /// The reads before admission: the eligible deployments and the routing
    /// plan (connection, `BEGIN` and statements alike).
    RouteLookup,
    /// The three-second deadline of a settlement the request waited for.
    ExplicitSettlement,
    /// The three-second deadline of the detached settlement of a cancelled
    /// request (its client is gone; no response carries the failure).
    CancellationSettlement,
}
impl Stage {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::PoolAcquireAndBegin,
        Self::GovernanceStorage,
        Self::RouteLookup,
        Self::ExplicitSettlement,
        Self::CancellationSettlement,
    ];
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PoolAcquireAndBegin => "pool_acquire_and_begin",
            Self::GovernanceStorage => "governance_storage",
            Self::RouteLookup => "route_lookup",
            Self::ExplicitSettlement => "explicit_settlement",
            Self::CancellationSettlement => "cancellation_settlement",
        }
    }
}

/// What kind of failure it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// No connection within the pool's acquire timeout: every pooled
    /// connection was busy, or a new one could not be opened in time.
    PoolTimeout,
    PoolClosed,
    /// SQLSTATE 55P03 (`lock_timeout`, or `NOWAIT`).
    LockTimeout,
    /// SQLSTATE 57014 (`statement_timeout`, or a cancel request).
    StatementTimeout,
    /// SQLSTATE 57P01 (the backend was terminated).
    BackendTerminated,
    /// SQLSTATE 40P01, after the deadlock re-runs were used up.
    Deadlock,
    /// SQLSTATE 25006: a write reached a read-only server (a standby, or a
    /// primary demoted by a failover).
    ReadOnlyTransaction,
    /// Any other error reported by the database, whatever its SQLSTATE.
    OtherDatabase,
    Io,
    Tls,
    Protocol,
    WorkerCrashed,
    /// Any other driver error.
    Other,
    /// The settlement deadline elapsed while the settlement waited for its
    /// in-process slot (the settlement gate).
    DeadlineInQueue,
    /// ... while it waited for a pooled connection (a busy pool, or a new
    /// connection not opened in time) or for `BEGIN`.
    DeadlineAcquiringConnection,
    /// ... while its transaction waited for the database (a lock or a statement).
    DeadlineInDatabase,
}
impl Cause {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 16] = [
        Self::PoolTimeout,
        Self::PoolClosed,
        Self::LockTimeout,
        Self::StatementTimeout,
        Self::BackendTerminated,
        Self::Deadlock,
        Self::ReadOnlyTransaction,
        Self::OtherDatabase,
        Self::Io,
        Self::Tls,
        Self::Protocol,
        Self::WorkerCrashed,
        Self::Other,
        Self::DeadlineInQueue,
        Self::DeadlineAcquiringConnection,
        Self::DeadlineInDatabase,
    ];
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PoolTimeout => "pool_timeout",
            Self::PoolClosed => "pool_closed",
            Self::LockTimeout => "lock_timeout",
            Self::StatementTimeout => "statement_timeout",
            Self::BackendTerminated => "backend_terminated",
            Self::Deadlock => "deadlock",
            Self::ReadOnlyTransaction => "read_only_transaction",
            Self::OtherDatabase => "other_database",
            Self::Io => "io",
            Self::Tls => "tls",
            Self::Protocol => "protocol",
            Self::WorkerCrashed => "worker_crashed",
            Self::Other => "other",
            Self::DeadlineInQueue => "deadline_in_queue",
            Self::DeadlineAcquiringConnection => "deadline_acquiring_connection",
            Self::DeadlineInDatabase => "deadline_in_database",
        }
    }
}

/// The error's native SQLSTATE, only when it is exactly five uppercase ASCII
/// letters or digits. For the test recorder and [`classify`]; never a label.
pub(crate) fn sqlstate(error: &sqlx::Error) -> Option<Cow<'_, str>> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .filter(|code| {
            code.len() == 5
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
}

/// The one classification of a driver error, shared with the test recorder
/// (`db::diagnostics`). Never returns one of the three deadline causes.
///
/// SQLSTATE 53300 (`too_many_connections`) is not listed: the server raises it
/// only at connection startup, and sqlx retries that refusal (like 57P03 and
/// a refused TCP connection) until the acquire timeout, so it always arrives
/// here as `PoolTimedOut`.
pub(crate) fn classify(error: &sqlx::Error) -> Cause {
    match error {
        sqlx::Error::Database(_) => match sqlstate(error).as_deref() {
            Some("55P03") => Cause::LockTimeout,
            Some("57014") => Cause::StatementTimeout,
            Some("57P01") => Cause::BackendTerminated,
            Some("40P01") => Cause::Deadlock,
            Some("25006") => Cause::ReadOnlyTransaction,
            _ => Cause::OtherDatabase,
        },
        sqlx::Error::PoolTimedOut => Cause::PoolTimeout,
        sqlx::Error::PoolClosed => Cause::PoolClosed,
        sqlx::Error::Io(_) => Cause::Io,
        sqlx::Error::Tls(_) => Cause::Tls,
        sqlx::Error::Protocol(_) => Cause::Protocol,
        sqlx::Error::WorkerCrashed => Cause::WorkerCrashed,
        _ => Cause::Other,
    }
}

/// A failed start of an accounting transaction.
pub(crate) fn begin_error(error: &sqlx::Error) {
    record(Stage::PoolAcquireAndBegin, classify(error));
}

/// A failed statement of an accounting transaction.
pub(crate) fn storage_error(error: &sqlx::Error) {
    record(Stage::GovernanceStorage, classify(error));
}

/// A failed statement that was run straight on the pool, outside a
/// transaction: without a connection (a pool timeout or a closed pool) it
/// never started, so that is the same failure as a failed start; anything
/// else is counted as a failed statement, including any other error raised
/// while a new connection was being opened for it (a known limit).
pub(crate) fn pool_statement_error(error: &sqlx::Error) {
    match error {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => begin_error(error),
        _ => storage_error(error),
    }
}

/// A failed read before admission (eligible deployments, routing plan).
pub(crate) fn route_lookup_error(error: &sqlx::Error) {
    record(Stage::RouteLookup, classify(error));
}

/// What a settlement is waiting on, for the cause of an elapsed deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waiting {
    /// Its in-process slot (the settlement gate).
    Queue,
    /// A pooled connection and `BEGIN`.
    Connection,
    /// The database, inside its transaction.
    Database,
}

tokio::task_local! {
    /// What the settlement inside a deadline wrapper below is waiting on.
    static WAITING: Cell<Waiting>;
}

/// Mark what the running settlement waits on next: one store into a
/// task-local cell (no allocation, no lock). Outside the two settlement
/// deadline wrappers below there is nothing to mark.
pub(crate) fn settlement_waiting(on: Waiting) {
    let _ = WAITING.try_with(|waiting| waiting.set(on));
}

/// An accounting transaction that was still a deadlock victim on its last
/// permitted run (`governance::locks::retry_deadlocks`).
pub(crate) fn deadlock_exhausted() {
    record(Stage::GovernanceStorage, Cause::Deadlock);
}

/// Observes the unchanged settlement timeout future of a request that waits
/// for it; it never constructs or extends a deadline.
pub(crate) async fn explicit_settlement<F, T>(future: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = Result<T, tokio::time::error::Elapsed>>,
{
    settlement(Stage::ExplicitSettlement, future).await
}

/// As [`explicit_settlement`], for the detached settlement of a cancelled request.
pub(crate) async fn cancellation_settlement<F, T>(
    future: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = Result<T, tokio::time::error::Elapsed>>,
{
    settlement(Stage::CancellationSettlement, future).await
}

async fn settlement<F, T>(stage: Stage, future: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = Result<T, tokio::time::error::Elapsed>>,
{
    // A settlement queues first; it marks each later wait itself
    // (`governance::finish_unobserved`).
    WAITING
        .scope(Cell::new(Waiting::Queue), async {
            let result = future.await;
            if result.is_err() {
                let cause = match WAITING.with(Cell::get) {
                    Waiting::Queue => Cause::DeadlineInQueue,
                    Waiting::Connection => Cause::DeadlineAcquiringConnection,
                    Waiting::Database => Cause::DeadlineInDatabase,
                };
                record(stage, cause);
            }
            result
        })
        .await
}

/// The only writer of the signal. The types bound it to 5 stages and 16
/// causes, so at most 80 label combinations. The callers above narrow that:
/// the three SQL stages take only the thirteen driver causes and the two
/// settlement stages only the three deadline causes (at most 45), and not
/// every one of those occurs in practice.
fn record(stage: Stage, cause: Cause) {
    record_to(&crate::metrics::METRICS, stage, cause);
}

/// [`record`] on the given metrics (the process-wide instance outside tests).
fn record_to(metrics: &crate::metrics::Metrics, stage: Stage, cause: Cause) {
    metrics.observe_accounting_failure(stage, cause);
    tracing::warn!(
        stage = stage.as_str(),
        cause = cause.as_str(),
        "accounting unavailable"
    );
    // Test builds also hand the pair to the request that owns the capture.
    #[cfg(test)]
    if let Some(handle) = super::diagnostics::current() {
        handle.signal((stage, cause));
    }
}

#[cfg(test)]
pub(crate) mod tests;
