//! #281: transaction-local isolation, independent of operator login defaults.
use super::*;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

/// Change only the sqlx-created disposable database, then connect anew: existing
/// sessions do not pick up ALTER DATABASE / ALTER ROLE ... IN DATABASE defaults.
/// Use the actual login role, not SET ROLE (which does not apply login defaults).
async fn elevated_pool(pool: &PgPool, level: &str, role_default: bool) -> PgPool {
    let (database, role): (String, String) =
        sqlx::query_as("SELECT current_database(),current_user")
            .fetch_one(pool)
            .await
            .unwrap();
    let database = database.replace('"', "\"\"");
    let role = role.replace('"', "\"\"");
    let sql = if role_default {
        // Make the two settings disagree so the test proves login precedence.
        format!(
            "ALTER DATABASE \"{database}\" SET default_transaction_isolation='read committed'; \
             ALTER ROLE \"{role}\" IN DATABASE \"{database}\" SET default_transaction_isolation='{level}'"
        )
    } else {
        format!("ALTER DATABASE \"{database}\" SET default_transaction_isolation='{level}'")
    };
    sqlx::raw_sql(&sql).execute(pool).await.unwrap();
    PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}

async fn start(pool: &PgPool, timeout_setup: bool) -> Transaction<'static, Postgres> {
    if timeout_setup {
        begin_with_setup(pool, "SET LOCAL statement_timeout='10s'")
            .await
            .unwrap()
    } else {
        begin(pool).await.unwrap()
    }
}

async fn baseline_ignores_defaults(pool: &PgPool, role_default: bool, timeout_setup: bool) {
    let mut observed = Vec::new();
    for level in ["repeatable read", "serializable"] {
        let fresh = elevated_pool(pool, level, role_default).await;
        let before: (i32, String, String) = sqlx::query_as(
            "SELECT pg_backend_pid(),current_setting('default_transaction_isolation'),current_setting('transaction_isolation')",
        )
        .fetch_one(&fresh)
        .await
        .unwrap();
        assert_eq!((&*before.1, &*before.2), (level, level));
        let mut tx = start(&fresh, timeout_setup).await;
        let (default, isolation, read_only): (String, String, String) = sqlx::query_as(
            "SELECT current_setting('default_transaction_isolation'),current_setting('transaction_isolation'),current_setting('transaction_read_only')",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(default, level, "do not rewrite the session default");
        if timeout_setup {
            let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(timeout, "10s");
        }
        tx.commit().await.unwrap();
        let after: (i32, String, String) = sqlx::query_as(
            "SELECT pg_backend_pid(),current_setting('default_transaction_isolation'),current_setting('transaction_isolation')",
        )
        .fetch_one(&fresh)
        .await
        .unwrap();
        assert_eq!(after, before, "same session must retain its login defaults");
        observed.push((isolation, read_only));
        fresh.close().await;
    }
    // Collect both defaults before asserting, so RED reports both inherited
    // levels instead of stopping after the first REPEATABLE READ transaction.
    assert_eq!(
        observed,
        vec![("read committed".to_owned(), "off".to_owned()); 2]
    );
}

#[sqlx::test(migrations = false)]
async fn begin_pins_read_committed_under_database_defaults(pool: PgPool) {
    baseline_ignores_defaults(&pool, false, false).await;
}

#[sqlx::test(migrations = false)]
async fn timeout_setup_pins_read_committed_under_database_defaults(pool: PgPool) {
    baseline_ignores_defaults(&pool, false, true).await;
}

#[sqlx::test(migrations = false)]
async fn begin_pins_read_committed_under_role_login_defaults(pool: PgPool) {
    baseline_ignores_defaults(&pool, true, false).await;
}

#[sqlx::test(migrations = false)]
async fn timeout_setup_pins_read_committed_under_role_login_defaults(pool: PgPool) {
    baseline_ignores_defaults(&pool, true, true).await;
}

#[sqlx::test(migrations = false)]
async fn begin_preserves_session_read_only_policy(pool: PgPool) {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Both defaults change on one session opened before either change: a pool
    // connection opened after the read-only default would refuse the second
    // ALTER DATABASE with 25006 (#329). Only new sessions see the defaults.
    let database = database.replace('"', "\"\"");
    let mut session = pool.acquire().await.unwrap();
    sqlx::raw_sql(&format!(
        "ALTER DATABASE \"{database}\" SET default_transaction_isolation='serializable'; \
         ALTER DATABASE \"{database}\" SET default_transaction_read_only=on"
    ))
    .execute(&mut *session)
    .await
    .unwrap();
    drop(session);
    let fresh = PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    for timeout_setup in [false, true] {
        let mut tx = start(&fresh, timeout_setup).await;
        let settings: (String, String, String) = sqlx::query_as(
            "SELECT current_setting('default_transaction_read_only'),current_setting('transaction_read_only'),current_setting('transaction_isolation')",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            settings,
            ("on".into(), "on".into(), "read committed".into())
        );
        tx.commit().await.unwrap();
    }
    fresh.close().await;
}

#[sqlx::test(migrations = false)]
async fn explicit_reporting_snapshot_keeps_its_isolation_and_old_rows(pool: PgPool) {
    sqlx::raw_sql(
        "CREATE TABLE snapshot_probe(value integer NOT NULL); INSERT INTO snapshot_probe VALUES(0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (index, level) in ["read committed", "repeatable read", "serializable"]
        .into_iter()
        .enumerate()
    {
        let fresh = elevated_pool(&pool, level, false).await;
        let store = crate::store::Store::new(fresh.clone());
        let mut snapshot = store.snapshot().await.unwrap();
        let settings: (String, String, String) = sqlx::query_as(
            "SELECT current_setting('default_transaction_isolation'),current_setting('transaction_isolation'),current_setting('transaction_read_only')",
        )
        .fetch_one(&mut *snapshot)
        .await
        .unwrap();
        assert_eq!(
            settings,
            (level.into(), "repeatable read".into(), "on".into())
        );
        let old: i32 = sqlx::query_scalar("SELECT value FROM snapshot_probe")
            .fetch_one(&mut *snapshot)
            .await
            .unwrap();
        assert_eq!(old, index as i32);
        // Commit on a different connection after the snapshot's first read.
        sqlx::query("UPDATE snapshot_probe SET value=value+1")
            .execute(&pool)
            .await
            .unwrap();
        let still: i32 = sqlx::query_scalar("SELECT value FROM snapshot_probe")
            .fetch_one(&mut *snapshot)
            .await
            .unwrap();
        assert_eq!(still, old);
        snapshot.commit().await.unwrap();
        let mut next = store.snapshot().await.unwrap();
        let now: i32 = sqlx::query_scalar("SELECT value FROM snapshot_probe")
            .fetch_one(&mut *next)
            .await
            .unwrap();
        assert_eq!(now, old + 1);
        let denied = sqlx::query("INSERT INTO snapshot_probe VALUES(99)")
            .execute(&mut *next)
            .await
            .unwrap_err();
        assert_eq!(
            denied.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("25006")
        );
        next.rollback().await.unwrap();
        fresh.close().await;
    }
}

#[sqlx::test(migrations = false)]
async fn lock_then_read_sees_the_holders_commit_under_elevated_defaults(pool: PgPool) {
    sqlx::query("CREATE TABLE visibility_probe(value integer NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let mut observed = Vec::new();
    for level in ["repeatable read", "serializable"] {
        let fresh = elevated_pool(&pool, level, false).await;
        for timeout_setup in [false, true] {
            sqlx::query("TRUNCATE visibility_probe")
                .execute(&pool)
                .await
                .unwrap();
            let mut holder = begin(&pool).await.unwrap();
            let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *holder)
                .await
                .unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock(281)")
                .execute(&mut *holder)
                .await
                .unwrap();
            let mut waiter = start(&fresh, timeout_setup).await;
            // Establish the waiter's snapshot before it tries the held lock.
            let (waiter_pid, before): (i32, i64) =
                sqlx::query_as("SELECT pg_backend_pid(),(SELECT count(*) FROM visibility_probe)")
                    .fetch_one(&mut *waiter)
                    .await
                    .unwrap();
            assert_eq!(before, 0);
            assert_ne!(waiter_pid, holder_pid);
            let task = tokio::spawn(async move {
                sqlx::query("SELECT pg_advisory_xact_lock(281)")
                    .execute(&mut *waiter)
                    .await
                    .unwrap();
                // A NEW statement after lock grant, not the lock statement's
                // snapshot or a CTE: this must see the holder's committed row.
                let after: i64 = sqlx::query_scalar("SELECT count(*) FROM visibility_probe")
                    .fetch_one(&mut *waiter)
                    .await
                    .unwrap();
                waiter.commit().await.unwrap();
                after
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let blocked: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND $2=ANY(pg_blocking_pids(pid)))",
                    )
                    .bind(waiter_pid)
                    .bind(holder_pid)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if blocked {
                        break;
                    }
                    assert!(!task.is_finished(), "waiter completed without blocking");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("waiter never blocked on the holder's lock");
            sqlx::query("INSERT INTO visibility_probe VALUES(1)")
                .execute(&mut *holder)
                .await
                .unwrap();
            holder.commit().await.unwrap();
            let after = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("waiter did not finish after lock release")
                .unwrap();
            observed.push((level, timeout_setup, after));
        }
        fresh.close().await;
    }
    assert!(
        observed.iter().all(|(_, _, after)| *after == 1),
        "a post-lock statement used a stale snapshot: {observed:?}"
    );
}

// #370: these tests observe the real SQLx/PostgreSQL boundary, not HTTP status
// mapping. The injected pool is also the independent observer connection pool.
async fn diagnostic_target_pool(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}

fn diagnostic_event(
    snapshot: &diagnostics::Snapshot,
    stage: diagnostics::Stage,
) -> &diagnostics::Event {
    let mut matches = snapshot.events.iter().filter(|event| event.stage == stage);
    let event = matches.next().expect("missing diagnostic stage");
    assert!(matches.next().is_none(), "duplicate diagnostic stage");
    event
}

fn assert_diagnostic_capture_available(snapshot: &diagnostics::Snapshot) {
    assert!(!snapshot.unavailable, "capture must be available");
    assert_eq!(snapshot.dropped, 0, "capture must not lose events");
}

async fn wait_for_diagnostic_setup_lock(
    observer: &PgPool,
    target_pid: i32,
    holder_pid: i32,
    caller: &tokio::task::JoinHandle<sqlx::Result<Transaction<'static, Postgres>>>,
) {
    // This is a bounded state proof, not a delay or a retry of a failing test.
    // Each autocommit query gets a fresh statistics snapshot. Never infer that
    // setup is blocked merely from the last diagnostic stage seen.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(!caller.is_finished(), "caller finished before lock proof");
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND state='active' AND wait_event_type='Lock' AND wait_event='advisory' AND $2=ANY(pg_blocking_pids(pid)))",
            )
            .bind(target_pid)
            .bind(holder_pid)
            .fetch_one(observer)
            .await
            .unwrap();
            if blocked {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("setup never demonstrably blocked on the holder's advisory lock");
}

#[sqlx::test(migrations = false)]
async fn diagnostics_preserve_native_setup_division_error(pool: PgPool) {
    use diagnostics::{Handle, Outcome, SqlxKind, Stage, scope};

    let target = diagnostic_target_pool(&pool).await;
    let handle = Handle::new();
    let error = scope(handle.clone(), begin_with_setup(&target, "SELECT 1/0"))
        .await
        .unwrap_err();
    target.close().await;
    assert!(matches!(&error, sqlx::Error::Database(_)));
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("22012"),
        "diagnostics must return the native SQLx error, not an HTTP 503"
    );
    let snapshot = handle.snapshot();
    assert_diagnostic_capture_available(&snapshot);
    assert_eq!(snapshot.pending, 0);
    let begin = diagnostic_event(&snapshot, Stage::PoolAcquireAndBegin);
    assert_eq!(begin.outcome, Outcome::Success);
    assert_eq!(begin.sqlx_error, None);
    assert_eq!(begin.sqlstate, None);
    let setup = diagnostic_event(&snapshot, Stage::Setup);
    assert_eq!(setup.outcome, Outcome::Error);
    assert_eq!(setup.sqlx_error, Some(SqlxKind::Database));
    assert_eq!(setup.sqlstate.as_deref(), Some("22012"));
    assert_eq!(
        snapshot.events.len(),
        2,
        "audit follows successful setup only"
    );
}

#[sqlx::test(migrations = false)]
async fn diagnostics_record_closed_pool_without_sqlstate(pool: PgPool) {
    use diagnostics::{Handle, Outcome, SqlxKind, Stage, scope};

    let target = diagnostic_target_pool(&pool).await;
    target.close().await;
    let handle = Handle::new();
    let error = scope(handle.clone(), begin(&target)).await.unwrap_err();
    assert!(matches!(error, sqlx::Error::PoolClosed));
    let snapshot = handle.snapshot();
    assert_diagnostic_capture_available(&snapshot);
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.events.len(), 1);
    let event = diagnostic_event(&snapshot, Stage::PoolAcquireAndBegin);
    assert_eq!(event.outcome, Outcome::Error);
    assert_eq!(event.sqlx_error, Some(SqlxKind::PoolClosed));
    assert_eq!(event.sqlstate, None);
}

#[sqlx::test(migrations = false)]
async fn diagnostics_survive_caller_abort_and_detached_setup_rolls_back(pool: PgPool) {
    use diagnostics::{Handle, Outcome, Stage, scope};

    sqlx::query("CREATE TABLE diagnostic_probe(value integer NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let target = diagnostic_target_pool(&pool).await;
    let target_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&target)
        .await
        .unwrap();
    let mut holder = begin(&pool).await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    assert_ne!(target_pid, holder_pid);
    sqlx::query("SELECT pg_advisory_xact_lock(370)")
        .execute(&mut *holder)
        .await
        .unwrap();

    let handle = Handle::new();
    let caller_handle = handle.clone();
    let caller_pool = target.clone();
    let caller = tokio::spawn(async move {
        scope(
            caller_handle,
            begin_with_setup(
                &caller_pool,
                "INSERT INTO diagnostic_probe VALUES (1); SELECT pg_advisory_xact_lock(370)",
            ),
        )
        .await
    });
    wait_for_diagnostic_setup_lock(&pool, target_pid, holder_pid, &caller).await;
    caller.abort();
    let cancelled = tokio::time::timeout(Duration::from_secs(10), caller)
        .await
        .expect("caller did not acknowledge abort")
        .unwrap_err();
    let caller_was_cancelled = cancelled.is_cancelled();

    // Retain evidence now, but defer every diagnostic assertion until after
    // unlock, connection release/rollback proof, and pool cleanup. Pre-hook RED
    // must not strand the detached setup behind our advisory lock.
    let blocked = handle.snapshot();
    holder.commit().await.unwrap();
    // Acquiring the only connection is a completion/release barrier, not a
    // timing guess. SQLx must flush the abandoned transaction's rollback.
    let mut reused = tokio::time::timeout(Duration::from_secs(10), target.acquire())
        .await
        .expect("detached task never released its connection")
        .unwrap();
    let idle_without_transaction: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND state='idle' AND xact_start IS NULL)",
    )
    .bind(target_pid)
    .fetch_one(&pool)
    .await
    .unwrap();
    let reused_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *reused)
        .await
        .unwrap();
    let rolled_back: i64 = sqlx::query_scalar("SELECT count(*) FROM diagnostic_probe")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO diagnostic_probe VALUES (2)")
        .execute(&mut *reused)
        .await
        .unwrap();
    let visible: Vec<i32> = sqlx::query_scalar("SELECT value FROM diagnostic_probe ORDER BY value")
        .fetch_all(&pool)
        .await
        .unwrap();
    let finished = handle.snapshot();
    drop(reused);
    target.close().await;

    assert!(caller_was_cancelled);
    assert!(
        idle_without_transaction,
        "released backend retained a transaction"
    );
    assert_eq!(
        reused_pid, target_pid,
        "replacement connection hides contamination"
    );
    assert_eq!(rolled_back, 0, "abandoned setup insert must roll back");
    assert_eq!(visible, vec![2], "same connection must really autocommit");
    assert_diagnostic_capture_available(&blocked);
    assert!(blocked.pending > 0, "detached setup must remain observable");
    assert_eq!(
        diagnostic_event(&blocked, Stage::PoolAcquireAndBegin).outcome,
        Outcome::Success
    );
    assert!(
        !blocked
            .events
            .iter()
            .any(|event| event.stage == Stage::Setup)
    );
    assert_diagnostic_capture_available(&finished);
    assert_eq!(finished.pending, 0);
    let setup = diagnostic_event(&finished, Stage::Setup);
    assert_eq!(
        setup.outcome,
        Outcome::Success,
        "caller abort must not cancel setup"
    );
    assert_eq!(setup.sqlx_error, None);
    assert_eq!(setup.sqlstate, None);
}
