//! Lock-free read snapshots and the optional reporting replica (scale plan P1).
//!
//! Reports, usage, logs, `/me` and `/v1/models` never take the catalog
//! advisory lock or the installation row lock. Each runs in one
//! `REPEATABLE READ READ ONLY` transaction: its authorization checks (live
//! platform role, workspace membership/ownership, key revalidation) and its
//! data queries all see the same database snapshot, without row locks, so
//! they neither wait for admission, settlement or management writes nor make
//! them wait.
//!
//! **Bounded staleness.** Such a read is linearized at its snapshot (its first
//! query), not at its end. A revocation (membership, role, user, key or
//! workspace disable) committed while the read is running does not affect
//! that read, which may still return data the caller was entitled to when it
//! started; the window is bounded by the read's duration (statement timeout
//! and handler deadline: 10 s). Every later read takes a new snapshot and sees
//! the revocation. Inference admission is unchanged: it revalidates the key,
//! user, grants and workspace live under its own locks, so revocation still
//! blocks all new upstream work immediately.
//!
//! **Reporting replica.** When `GATEWAY_REPORTING_DATABASE_URL` is set,
//! reports, usage and logs (never `/me`, `/v1/models`, admission, settlement
//! or any write) authorize on the primary first, in their own snapshot, then
//! read the data from a snapshot on the reporting pool. Authorization is
//! therefore as fresh as on the primary; the data is as old as the replica's
//! replay position, at most `GATEWAY_REPORTING_MAX_LAG_SECONDS` behind (a
//! replica further behind, unreachable or failing is skipped, and the read
//! uses the primary snapshot it already authorized in).
use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};

use crate::store::Store;

/// Snapshot isolation, read only, bounded statement time (the handlers also
/// enforce a 10 s deadline). One round trip; must start the transaction.
const SNAPSHOT: &str =
    "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY; SET LOCAL statement_timeout='10s'";

/// Replay lag in seconds of the connected server: 0 on a primary or a caught
/// up standby, otherwise the age of the last replayed commit (conservative
/// after an idle period), `+Infinity` when unknown.
const REPLAY_LAG: &str = "SELECT CASE WHEN NOT pg_is_in_recovery() THEN 0::float8 WHEN pg_last_wal_receive_lsn() IS NOT NULL AND pg_last_wal_receive_lsn()=pg_last_wal_replay_lsn() THEN 0::float8 ELSE coalesce(extract(epoch FROM clock_timestamp()-pg_last_xact_replay_timestamp())::float8,'Infinity'::float8) END";

/// Both statements in one simple-query round trip, started through
/// [`crate::db`] so a cancelled request can never leave the pooled connection
/// inside an untracked transaction (which would make a later snapshot's
/// `SET TRANSACTION` fail with 25001).
async fn begin(pool: &PgPool) -> sqlx::Result<Transaction<'static, Postgres>> {
    crate::db::begin_with_setup(pool, SNAPSHOT).await
}

/// Whether a reporting server with this replay lag may serve reads.
pub(crate) fn fresh_enough(lag_seconds: f64, max_lag: Duration) -> bool {
    lag_seconds.is_finite() && lag_seconds <= max_lag.as_secs_f64()
}

impl Store {
    /// Attach the optional reporting pool (`None` keeps the primary only).
    pub fn with_reporting(mut self, pool: Option<PgPool>, max_lag: Duration) -> Self {
        self.reporting = pool;
        self.reporting_max_lag = max_lag;
        self
    }

    /// A lock-free `REPEATABLE READ READ ONLY` snapshot on the primary. Use
    /// it for authorization and reads that must not wait on (or block) the
    /// installation or catalog locks. It cannot write.
    pub async fn snapshot(&self) -> sqlx::Result<Transaction<'static, Postgres>> {
        begin(&self.pool).await
    }

    /// The transaction to read report/usage/log data in, after the caller
    /// authorized in `authorized` (from [`Store::snapshot`]).
    ///
    /// Without a reporting pool this is `authorized` itself, so authorization
    /// and data share one snapshot. With one, a fresh-enough replica snapshot
    /// replaces it (the primary snapshot is released); on any replica error
    /// or excessive lag the primary snapshot is returned instead.
    pub async fn reporting(
        &self,
        authorized: Transaction<'static, Postgres>,
    ) -> sqlx::Result<Transaction<'static, Postgres>> {
        let Some(pool) = &self.reporting else {
            return Ok(authorized);
        };
        let replica = async {
            let mut tx = begin(pool).await?;
            let lag: f64 = sqlx::query_scalar(REPLAY_LAG).fetch_one(&mut *tx).await?;
            Ok::<_, sqlx::Error>((tx, lag))
        }
        .await;
        match replica {
            Ok((tx, lag)) if fresh_enough(lag, self.reporting_max_lag) => {
                authorized.commit().await?;
                Ok(tx)
            }
            Ok((_, lag)) => {
                tracing::warn!(
                    lag_seconds = lag,
                    "reporting replica is behind; reading from the primary"
                );
                Ok(authorized)
            }
            Err(error) => {
                // No URL or credentials in the message: only the error kind.
                tracing::warn!(
                    error = error_kind(&error),
                    "reporting replica unavailable; reading from the primary"
                );
                Ok(authorized)
            }
        }
    }
}

fn error_kind(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::PoolTimedOut => "pool_timed_out",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Database(_) => "database",
        sqlx::Error::Protocol(_) => "protocol",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_freshness_bound() {
        let max = Duration::from_secs(30);
        assert!(fresh_enough(0.0, max));
        assert!(fresh_enough(30.0, max));
        assert!(!fresh_enough(30.5, max));
        assert!(!fresh_enough(f64::INFINITY, max));
        assert!(!fresh_enough(f64::NAN, max));
    }
}
