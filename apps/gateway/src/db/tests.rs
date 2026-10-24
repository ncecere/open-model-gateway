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
