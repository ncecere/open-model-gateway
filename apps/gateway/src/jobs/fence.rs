//! The claim fence of a gateway-run batch (migration 0081, #638).
//!
//! A runner owns a batch while `(runner_id, runner_epoch)` on the job row are
//! the pair it was given when it claimed the batch; the claim bumps the epoch,
//! so a later claim by any runner, including a new run of the same process,
//! ends the earlier run's ownership. Every runner write to the batch's rows
//! starts its transaction by locking the job row with that pair in the
//! predicate: `FOR KEY SHARE` when it writes other rows only, `FOR NO KEY
//! UPDATE` when it also changes the job row. A claim locks the row `FOR
//! UPDATE` (`SKIP LOCKED`), which conflicts with both, so it skips a batch
//! with a write in flight and takes it on a later tick; a write that starts
//! after a claim finds no row and is refused ([`FenceError::Stale`]). The
//! runner then writes nothing more to the batch. Every takeover must lock the
//! row `FOR UPDATE`, as the claim does: a plain `UPDATE` takes only `NO KEY
//! UPDATE`, which `KEY SHARE` does not exclude.
//!
//! Lock order: the job row is the first row lock of every runner transaction,
//! before the route lock, line rows, segment rows and, in governance, the
//! reservation and totals rows (docs/governance.md). Transactions that change
//! the job row take it `FOR NO KEY UPDATE` at the start, so no transaction
//! upgrades a lock it already holds; `KEY SHARE` and `NO KEY UPDATE` do not
//! conflict, so admissions, segment writes and counter updates of one run do
//! not queue behind each other on it.
//!
//! A frozen runner inside a fenced transaction would hold the row against
//! every claim: each fenced transaction therefore ends the session after
//! [`IDLE_IN_TRANSACTION`] without a statement, which releases the lock.
use sqlx::{Postgres, Transaction, postgres::PgArguments, query::Query};
use uuid::Uuid;

use super::store::{Finished, JobRow, finish_batch_sql, job_storage};
use crate::store::Store;

/// What a runner holds for one claim of a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BatchFence {
    pub(crate) job: Uuid,
    pub(crate) runner: Uuid,
    pub(crate) epoch: i64,
}

/// A fenced write that did not happen.
#[derive(Debug)]
pub(crate) enum FenceError {
    /// The claim ended: the batch belongs to another claim now.
    Stale,
    /// A statement failed (counted where it ran).
    Storage,
}
impl From<sqlx::Error> for FenceError {
    fn from(_: sqlx::Error) -> Self {
        Self::Storage
    }
}
/// What a lease renewal found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Renewed {
    /// The claim is current; whether a cancel was requested.
    Held { cancel: bool },
    /// The batch is settled: nothing is left to renew.
    Settled,
    /// The claim ended.
    Fenced,
}

pub(crate) type FenceResult<T> = Result<T, FenceError>;

/// The longest a fenced transaction may sit between statements.
pub(crate) const IDLE_IN_TRANSACTION: &str = "30s";

/// How a transaction holds the job row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    /// Writes other rows only (`FOR KEY SHARE`).
    Rows,
    /// Also changes the job row.
    Update,
}

impl BatchFence {
    /// Whether the claim is still current, holding the job row until `tx` ends
    /// when it is. The first statement of every runner write transaction.
    /// Database errors are not counted here: the caller maps them.
    pub(crate) async fn hold(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hold: Hold,
    ) -> Result<bool, sqlx::Error> {
        // The select-list `set_config` runs for the locked row: a refused
        // fence rolls back, so the timeout only matters when it is held.
        let sql = match hold {
            Hold::Rows => {
                "SELECT set_config('idle_in_transaction_session_timeout',$4,true) FROM async_jobs WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 FOR KEY SHARE"
            }
            Hold::Update => {
                "SELECT set_config('idle_in_transaction_session_timeout',$4,true) FROM async_jobs WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 FOR NO KEY UPDATE"
            }
        };
        Ok(sqlx::query_scalar::<_, String>(sql)
            .bind(self.job)
            .bind(self.runner)
            .bind(self.epoch)
            .bind(IDLE_IN_TRANSACTION)
            .fetch_optional(&mut **tx)
            .await?
            .is_some())
    }

    /// [`Self::hold`] for the job module: a failed statement is counted once.
    pub(crate) async fn hold_counted(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hold: Hold,
    ) -> FenceResult<()> {
        if self.hold(tx, hold).await.map_err(job_storage)? {
            Ok(())
        } else {
            Err(FenceError::Stale)
        }
    }

    async fn begin(
        &self,
        store: &Store,
        hold: Hold,
    ) -> FenceResult<Transaction<'static, Postgres>> {
        let mut tx = crate::db::begin(&store.pool).await.map_err(job_storage)?;
        self.hold_counted(&mut tx, hold).await?;
        Ok(tx)
    }

    /// Whether the claim is still current (a read; it fences nothing).
    pub(crate) async fn current(&self, store: &Store) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM async_jobs WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3)")
            .bind(self.job)
            .bind(self.runner)
            .bind(self.epoch)
            .fetch_one(&store.pool)
            .await
            .map_err(job_storage)
    }

    /// A statement whose first three parameters are the fence's.
    fn query<'q>(&self, sql: &'q str) -> Query<'q, Postgres, PgArguments> {
        sqlx::query(sql)
            .bind(self.job)
            .bind(self.runner)
            .bind(self.epoch)
    }

    /// One statement on the job row alone: the predicate is the fence (the
    /// update re-checks it on the locked row), so no transaction is needed.
    async fn update(
        &self,
        store: &Store,
        query: Query<'_, Postgres, PgArguments>,
    ) -> FenceResult<()> {
        let done = query
            .execute(&store.pool)
            .await
            .map_err(job_storage)?
            .rows_affected();
        if done == 0 {
            return Err(FenceError::Stale);
        }
        Ok(())
    }

    /// Renew the lease.
    pub(crate) async fn renew(
        &self,
        store: &Store,
        lease_seconds: f64,
    ) -> Result<Renewed, sqlx::Error> {
        let row: Option<(bool, bool)> = sqlx::query_as("UPDATE async_jobs SET runner_lease_until=clock_timestamp()+make_interval(secs=>$4) WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 RETURNING cancel_requested_at IS NOT NULL,settled_at IS NOT NULL")
            .bind(self.job).bind(self.runner).bind(self.epoch).bind(lease_seconds)
            .fetch_optional(&store.pool).await.map_err(job_storage)?;
        Ok(match row {
            None => Renewed::Fenced,
            Some((_, true)) => Renewed::Settled,
            Some((cancel, false)) => Renewed::Held { cancel },
        })
    }

    /// Give the batch up at once (a failed run), so another runner need not
    /// wait for the lease term.
    pub(crate) async fn release(&self, store: &Store) -> FenceResult<()> {
        self.update(
            store,
            self.query("UPDATE async_jobs SET runner_lease_until=NULL WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3"),
        )
        .await
    }

    pub(crate) async fn mark_running(&self, store: &Store) -> FenceResult<()> {
        // No row is also the normal case of a resumed batch that is running
        // already; the first fenced write after this finds a lost claim.
        let query = self.query("UPDATE async_jobs SET state='in_progress',upstream_status=CASE WHEN cancel_requested_at IS NULL THEN 'in_progress' ELSE upstream_status END,in_progress_at=coalesce(in_progress_at,clock_timestamp()),last_progress_at=coalesce(last_progress_at,clock_timestamp()) WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND state='queued'");
        match self.update(store, query).await {
            Ok(_) | Err(FenceError::Stale) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Display status of an unfinished batch.
    pub(crate) async fn set_status(&self, store: &Store, status: &str) -> FenceResult<()> {
        let query = self.query("UPDATE async_jobs SET upstream_status=$4,finalizing_at=CASE WHEN $4='finalizing' THEN coalesce(finalizing_at,clock_timestamp()) ELSE finalizing_at END WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND state IN('queued','in_progress')");
        self.update(store, query.bind(status)).await
    }

    /// Lines finished since the last call.
    pub(crate) async fn progress(
        &self,
        store: &Store,
        completed: i32,
        failed: i32,
    ) -> FenceResult<()> {
        let query = self.query("UPDATE async_jobs SET request_completed=coalesce(request_completed,0)+$4,request_failed=coalesce(request_failed,0)+$5,last_progress_at=clock_timestamp() WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3");
        self.update(store, query.bind(completed).bind(failed)).await
    }

    /// Record the terminal state and result files. [`FenceError::Stale`] when
    /// the claim ended: nothing was recorded.
    pub(crate) async fn finish_batch(
        &self,
        store: &Store,
        f: &Finished<'_>,
    ) -> FenceResult<JobRow> {
        let row: Option<JobRow> = sqlx::query_as(&finish_batch_sql(true))
            .bind(self.job)
            .bind(self.runner)
            .bind(self.epoch)
            .bind(f.state.as_str())
            .bind(f.error_code)
            .bind(f.output_file)
            .bind(f.error_file)
            .bind(f.counts.map(|c| c.total as i32))
            .bind(f.counts.map(|c| c.completed as i32))
            .bind(f.counts.map(|c| c.failed as i32))
            .fetch_optional(&store.pool)
            .await
            .map_err(job_storage)?;
        match row {
            Some(row) => Ok(row),
            // Already finished by this claim, or the claim ended.
            None if self.current(store).await? => Ok(super::store::reload(store, self.job).await?),
            None => Err(FenceError::Stale),
        }
    }

    pub(crate) async fn mark_settled(&self, store: &Store) -> FenceResult<()> {
        // Nothing to do when it is settled already; a lost claim settles
        // nothing, and the claim that holds the batch settles it.
        let query = self.query("UPDATE async_jobs SET settled_at=clock_timestamp() WHERE id=$1 AND runner_id=$2 AND runner_epoch=$3 AND settled_at IS NULL AND state IN('completed','failed','cancelled','expired')");
        query.execute(&store.pool).await.map_err(job_storage)?;
        Ok(())
    }

    /// Claim a line's first attempt. Inserting the row is the claim, so a line
    /// can never run twice (even across runners).
    pub(crate) async fn claim_line(
        &self,
        store: &Store,
        workspace: Uuid,
        line_no: i32,
        execution: Uuid,
    ) -> FenceResult<bool> {
        let mut tx = self.begin(store, Hold::Rows).await?;
        let inserted = sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id) VALUES($1,$2,$3,'running',$4) ON CONFLICT DO NOTHING")
            .bind(self.job).bind(workspace).bind(line_no).bind(execution)
            .execute(&mut *tx).await.map_err(job_storage)?.rows_affected() == 1;
        tx.commit().await.map_err(job_storage)?;
        Ok(inserted)
    }

    /// Claim an explicit retry (a new attempt with its own execution).
    pub(crate) async fn retry_line(
        &self,
        store: &Store,
        line_no: i32,
        attempts: i16,
        execution: Uuid,
    ) -> FenceResult<bool> {
        let mut tx = self.begin(store, Hold::Rows).await?;
        let claimed = sqlx::query("UPDATE batch_lines SET state='running',attempts=attempts+1,execution_id=$4,status_code=NULL,error_code=NULL,finished_at=NULL WHERE job_id=$1 AND line_no=$2 AND state='failed' AND attempts=$3 AND segment IS NULL")
            .bind(self.job).bind(line_no).bind(attempts).bind(execution)
            .execute(&mut *tx).await.map_err(job_storage)?.rows_affected() == 1;
        tx.commit().await.map_err(job_storage)?;
        Ok(claimed)
    }

    /// Record a line's outcome. With `count` the batch's counters move in the
    /// same transaction, and only when this call is what moved the line out of
    /// `running` (a line already interrupted or finished is not counted
    /// twice). Returns whether the line changed.
    pub(crate) async fn finish_line(
        &self,
        store: &Store,
        line_no: i32,
        succeeded: bool,
        status_code: Option<u16>,
        error_code: Option<&str>,
        count: bool,
    ) -> FenceResult<bool> {
        let mut tx = self
            .begin(store, if count { Hold::Update } else { Hold::Rows })
            .await?;
        let changed = sqlx::query("UPDATE batch_lines SET state=$3,status_code=$4,error_code=$5,finished_at=clock_timestamp() WHERE job_id=$1 AND line_no=$2 AND state='running'")
            .bind(self.job).bind(line_no).bind(if succeeded { "succeeded" } else { "failed" })
            .bind(status_code.map(|s| s as i16)).bind(error_code)
            .execute(&mut *tx).await.map_err(job_storage)?.rows_affected() == 1;
        if count && changed {
            let (c, f) = if succeeded { (1, 0) } else { (0, 1) };
            count_lines(&mut tx, self.job, c, f).await?;
        }
        tx.commit().await.map_err(job_storage)?;
        Ok(changed)
    }

    /// A line a crashed or lost runner left `running` is `interrupted`: never
    /// executed again. Counted failed in the same transaction. Returns how
    /// many.
    pub(crate) async fn interrupt_running(&self, store: &Store) -> FenceResult<i32> {
        let mut tx = self.begin(store, Hold::Update).await?;
        let n = sqlx::query("UPDATE batch_lines SET state='interrupted',error_code='interrupted',finished_at=clock_timestamp() WHERE job_id=$1 AND state='running'")
            .bind(self.job).execute(&mut *tx).await.map_err(job_storage)?.rows_affected();
        let n = i32::try_from(n).unwrap_or(i32::MAX);
        if n > 0 {
            count_lines(&mut tx, self.job, 0, n).await?;
        }
        tx.commit().await.map_err(job_storage)?;
        Ok(n)
    }

    /// Record a stored result segment and the lines it holds, as the next
    /// segment number. The number is taken inside the fenced transaction, so
    /// one failed or lost insert never makes a later one collide.
    pub(crate) async fn insert_segment(
        &self,
        store: &Store,
        workspace: Uuid,
        file: Uuid,
        lines: &[i32],
    ) -> FenceResult<i32> {
        // A process that predates the epoch allocates its own numbers and can
        // take ours between the read and the insert: take the next one.
        let mut collisions = 0;
        loop {
            let mut tx = self.begin(store, Hold::Rows).await?;
            let seq: Result<i32, sqlx::Error> = sqlx::query_scalar("INSERT INTO batch_segments(job_id,workspace_id,seq,file_id,lines) SELECT $1,$2,coalesce(max(seq)+1,0),$3,$4 FROM batch_segments WHERE job_id=$1 RETURNING seq")
                .bind(self.job).bind(workspace).bind(file).bind(lines.len() as i32)
                .fetch_one(&mut *tx).await;
            let seq = match seq {
                Ok(seq) => seq,
                Err(e) if collisions < 3 && is_unique_violation(&e) => {
                    collisions += 1;
                    continue;
                }
                Err(e) => return Err(job_storage(e).into()),
            };
            sqlx::query("UPDATE batch_lines SET segment=$2 WHERE job_id=$1 AND line_no=ANY($3) AND segment IS NULL AND state<>'running'")
                .bind(self.job).bind(seq).bind(lines).execute(&mut *tx).await.map_err(job_storage)?;
            tx.commit().await.map_err(job_storage)?;
            return Ok(seq);
        }
    }
}

/// Add finished lines to the batch's counters (the job row is held).
async fn count_lines(
    tx: &mut Transaction<'_, Postgres>,
    job: Uuid,
    completed: i32,
    failed: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE async_jobs SET request_completed=coalesce(request_completed,0)+$2,request_failed=coalesce(request_failed,0)+$3,last_progress_at=clock_timestamp() WHERE id=$1")
        .bind(job).bind(completed).bind(failed)
        .execute(&mut **tx).await.map_err(job_storage)?;
    Ok(())
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().as_deref() == Some("23505"))
}
