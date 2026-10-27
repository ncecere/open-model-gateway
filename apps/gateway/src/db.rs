//! Cancellation-safe READ COMMITTED transaction start.
//!
//! sqlx 0.8 (`PgTransactionManager::begin`) counts a transaction only after
//! `BEGIN`'s reply arrives, and its drop guard rolls back only counted
//! transactions. A begin future dropped after `BEGIN` reached the server (a
//! client disconnect or navigation abort cancelling the handler, a deadline)
//! therefore returns the connection to the pool *inside* an untracked
//! server-side transaction. Later plain pool statements run in it (their
//! writes stay uncommitted and their row locks held until someone commits or
//! rolls back on that connection), and the next lock-free snapshot fails with
//! `25001 SET TRANSACTION ISOLATION LEVEL must be called before any query`.
//!
//! Every transaction therefore starts here: the begin (and any first setup
//! statement) runs to completion in its own task, so dropping the caller can
//! never interrupt it. A transaction nobody receives is dropped by the task,
//! which queues its `ROLLBACK`; the pool flushes it on release. Direct
//! `Pool::begin` and `Pool::begin_with` are rejected by `clippy.toml`
//! (`disallowed-methods`) outside this sanctioned call site.
use sqlx::{PgPool, Postgres, Transaction};

#[cfg(test)]
pub(crate) mod diagnostics;
#[cfg(all(test, feature = "integration-tests"))]
mod tests;

/// Begin a READ COMMITTED transaction, inheriting the session's read-only policy.
pub(crate) async fn begin(pool: &PgPool) -> sqlx::Result<Transaction<'static, Postgres>> {
    start(pool, None).await
}

/// Begin at READ COMMITTED and run `setup` (simple-query protocol, so it may hold
/// several statements, e.g. `SET TRANSACTION ...`) as its first statement,
/// both uncancellable.
pub(crate) async fn begin_with_setup(
    pool: &PgPool,
    setup: &'static str,
) -> sqlx::Result<Transaction<'static, Postgres>> {
    start(pool, Some(setup)).await
}

async fn start(
    pool: &PgPool,
    setup: Option<&'static str>,
) -> sqlx::Result<Transaction<'static, Postgres>> {
    let pool = pool.clone();
    let start = async move {
        #[allow(clippy::disallowed_methods)] // the one sanctioned call site
        let begin = pool.begin_with("BEGIN ISOLATION LEVEL READ COMMITTED");
        #[cfg(test)]
        let begin = diagnostics::sql(diagnostics::Stage::PoolAcquireAndBegin, begin);
        let mut tx = begin.await?;
        if let Some(sql) = setup {
            let setup = sqlx::Executor::execute(&mut *tx, sql);
            #[cfg(test)]
            let setup = diagnostics::sql(diagnostics::Stage::Setup, setup);
            setup.await?;
        }
        // Test builds audit the scoped lock order (migration 0027): every
        // gateway transaction that changes an authority scope must take its
        // exclusive lock up front, in canonical order.
        #[cfg(any(test, feature = "integration-tests"))]
        if std::env::var("GATEWAY_SCOPE_LOCK_AUDIT").as_deref() != Ok("off") {
            let audit = sqlx::Executor::execute(
                &mut *tx,
                "SELECT set_config('omg.scope_lock_audit','on',true)",
            );
            #[cfg(test)]
            let audit = diagnostics::sql(diagnostics::Stage::ScopeAudit, audit);
            audit.await?;
        }
        Ok(tx)
    };
    #[cfg(test)]
    let start = diagnostics::inherit(start);
    // Preserve the uncancellable owner task: no database await moves into the caller.
    let task = tokio::spawn(start);
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // Only on runtime shutdown.
        Err(_) => Err(sqlx::Error::WorkerCrashed),
    }
}
