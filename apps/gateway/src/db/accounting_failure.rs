//! Production signal for the cause of `503 accounting_unavailable` (#389).
//!
//! Each failed accounting operation increments
//! `gateway_accounting_failures_total{stage,cause}` once and writes one WARN
//! line carrying the same two fields. Both values come only from the closed
//! enums below: never SQL text, bind values, a raw SQLSTATE, a driver or
//! database message, an identifier or a credential. Nothing here runs when
//! accounting succeeds.
use std::{borrow::Cow, future::Future};

/// Where the accounting operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Starting an accounting transaction: taking a pooled connection and
    /// `BEGIN` (sqlx does not separate the two).
    PoolAcquireAndBegin,
    /// Any later statement of an accounting transaction, including `COMMIT`.
    GovernanceStorage,
    /// The three-second deadline of a settlement the request waited for.
    ExplicitSettlement,
    /// The three-second deadline of the detached settlement of a cancelled
    /// request (its client is gone; no response carries the failure).
    CancellationSettlement,
}
impl Stage {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [
        Self::PoolAcquireAndBegin,
        Self::GovernanceStorage,
        Self::ExplicitSettlement,
        Self::CancellationSettlement,
    ];
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PoolAcquireAndBegin => "pool_acquire_and_begin",
            Self::GovernanceStorage => "governance_storage",
            Self::ExplicitSettlement => "explicit_settlement",
            Self::CancellationSettlement => "cancellation_settlement",
        }
    }
}

/// What kind of failure it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// No pooled connection within the pool's acquire timeout.
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
    /// Any other error reported by the database, whatever its SQLSTATE.
    OtherDatabase,
    Io,
    Tls,
    Protocol,
    WorkerCrashed,
    /// Any other driver error.
    Other,
    /// The settlement deadline elapsed before the database answered.
    DeadlineExceeded,
}
impl Cause {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 13] = [
        Self::PoolTimeout,
        Self::PoolClosed,
        Self::LockTimeout,
        Self::StatementTimeout,
        Self::BackendTerminated,
        Self::Deadlock,
        Self::OtherDatabase,
        Self::Io,
        Self::Tls,
        Self::Protocol,
        Self::WorkerCrashed,
        Self::Other,
        Self::DeadlineExceeded,
    ];
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PoolTimeout => "pool_timeout",
            Self::PoolClosed => "pool_closed",
            Self::LockTimeout => "lock_timeout",
            Self::StatementTimeout => "statement_timeout",
            Self::BackendTerminated => "backend_terminated",
            Self::Deadlock => "deadlock",
            Self::OtherDatabase => "other_database",
            Self::Io => "io",
            Self::Tls => "tls",
            Self::Protocol => "protocol",
            Self::WorkerCrashed => "worker_crashed",
            Self::Other => "other",
            Self::DeadlineExceeded => "deadline_exceeded",
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
/// (`db::diagnostics`). Never returns [`Cause::DeadlineExceeded`].
pub(crate) fn classify(error: &sqlx::Error) -> Cause {
    match error {
        sqlx::Error::Database(_) => match sqlstate(error).as_deref() {
            Some("55P03") => Cause::LockTimeout,
            Some("57014") => Cause::StatementTimeout,
            Some("57P01") => Cause::BackendTerminated,
            Some("40P01") => Cause::Deadlock,
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
    let result = future.await;
    if result.is_err() {
        record(stage, Cause::DeadlineExceeded);
    }
    result
}

/// The only writer of the signal. Callers above fix which pairs can occur:
/// the two SQL stages with the twelve driver causes, and the two settlement
/// stages with `deadline_exceeded` (26 label combinations).
fn record(stage: Stage, cause: Cause) {
    crate::metrics::METRICS.observe_accounting_failure(stage, cause);
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
