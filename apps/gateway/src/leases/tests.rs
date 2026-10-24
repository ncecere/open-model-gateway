//! Work leases (scale plan P5, migration 0029) on real PostgreSQL with three
//! simulated replicas (three holders on separate connections): election,
//! handover after a holder dies, fencing of a stale leader, no duplicate
//! singleton runs, and `SKIP LOCKED` queues shared without duplicates.
use super::*;
use crate::{
    governance::tests::db::{fixture, request},
    store::Store,
};
use std::sync::atomic::{AtomicUsize, Ordering};

const SHORT: Duration = Duration::from_secs(2);

async fn replica(pool: &PgPool) -> Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    Store::new(pool)
}

fn holders(n: usize) -> Vec<Arc<Leases>> {
    (0..n).map(|_| Arc::new(Leases::with_ttl(SHORT))).collect()
}

/// Concurrent elections: every lease has exactly one holder; renewals keep
/// the holder and its epoch.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn exactly_one_of_three_replicas_holds_each_lease(pool: PgPool) {
    let replicas = holders(3);
    let stores = [
        replica(&pool).await,
        replica(&pool).await,
        replica(&pool).await,
    ];
    for _ in 0..3 {
        let counts = futures::future::join_all(
            replicas
                .iter()
                .zip(&stores)
                .map(|(l, s)| l.renew_once(&s.pool)),
        )
        .await;
        assert_eq!(counts.iter().sum::<usize>(), Lease::ALL.len(), "{counts:?}");
    }
    for lease in Lease::ALL {
        let held: Vec<Fence> = replicas.iter().filter_map(|r| r.held(lease)).collect();
        assert_eq!(held.len(), 1, "{lease:?}");
        let (holder, epoch): (Uuid, i64) =
            sqlx::query_as("SELECT holder,epoch FROM work_leases WHERE name=$1")
                .bind(lease.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((holder, epoch), (held[0].holder, held[0].epoch));
        assert_eq!(epoch, 1, "renewal keeps the term");
    }
}

/// [`SHORT`] as a database-timestamp difference.
fn term() -> chrono::Duration {
    chrono::Duration::from_std(SHORT).unwrap()
}

/// Whether the database's clock has not yet reached `instant`. Expiry is the
/// database's decision (`omg_lease_acquire` compares against
/// `clock_timestamp()`), so the test reads the same clock rather than timing
/// the test process, whose pace under load says nothing about the term.
async fn db_before(pool: &PgPool, instant: chrono::DateTime<chrono::Utc>) -> bool {
    sqlx::query_scalar("SELECT clock_timestamp() < $1")
        .bind(instant)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Block inside the database until its clock has reached `instant`.
async fn wait_db_until(pool: &PgPool, instant: chrono::DateTime<chrono::Utc>) {
    while db_before(pool, instant).await {
        sqlx::query(
            "SELECT pg_sleep(GREATEST(EXTRACT(EPOCH FROM ($1 - clock_timestamp())), 0.001))",
        )
        .bind(instant)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// One `work_leases` row as the database recorded it: (name, holder, epoch,
/// acquired_at, expires_at).
type LeaseRow = (
    String,
    Option<Uuid>,
    i64,
    Option<chrono::DateTime<chrono::Utc>>,
    chrono::DateTime<chrono::Utc>,
);

async fn lease_rows(pool: &PgPool) -> Vec<LeaseRow> {
    sqlx::query_as("SELECT name,holder,epoch,acquired_at,expires_at FROM work_leases ORDER BY name")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// No lease changed hands before its original term ended. Compares every row
/// with `original`; a row whose holder or epoch changed must have been
/// acquired (database `acquired_at`) no earlier than that lease's original
/// `expires_at`. Unlike a check on how the clock stands after an attempt, this
/// reads the acquisition time the database itself recorded, so an early
/// acquisition of any lease is caught however late the check runs. Call it
/// right after every attempt, before another attempt can overwrite the row.
async fn assert_no_early_takeover(pool: &PgPool, original: &[LeaseRow], when: &str) {
    let now = lease_rows(pool).await;
    assert_eq!(now.len(), original.len(), "{when}: lease set changed");
    for (was, is) in original.iter().zip(&now) {
        assert_eq!(was.0, is.0, "{when}: lease set changed");
        if (was.1, was.2) == (is.1, is.2) {
            continue;
        }
        let acquired =
            is.3.unwrap_or_else(|| panic!("{when}: {} changed hands without acquired_at", is.0));
        assert!(
            acquired >= was.4,
            "{when}: lease {} was taken at {acquired} before its term ended at {}",
            is.0,
            was.4
        );
    }
}

/// A holder that dies (stops renewing) loses its leases once the term
/// expires; another replica takes over with a larger epoch, and the former
/// holder's fenced writes fail.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_dead_holder_is_replaced_after_expiry_and_fenced(pool: PgPool) {
    let replicas = holders(3);
    let store = replica(&pool).await;
    assert_eq!(replicas[0].renew_once(&pool).await, Lease::ALL.len());
    let old = replicas[0].held(Lease::Maintenance).unwrap();
    // The old term as the database recorded it: every timing assertion below
    // is made on these database timestamps, not on how fast this process ran.
    let (old_acquired, old_expires): (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT acquired_at,expires_at FROM work_leases WHERE name='maintenance'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(old_expires >= old_acquired + term());
    // Every lease's original term, for all leases (not just maintenance).
    let original = lease_rows(&pool).await;
    assert_eq!(original.len(), Lease::ALL.len());
    for row in &original {
        assert_eq!(row.1, Some(replicas[0].holder()), "{}", row.0);
    }
    // Each lease has its own expiry: a survivor may acquire an expired lease
    // while maintenance is still current. Check every lease's recorded
    // acquisition time against its own original expiry after each attempt,
    // so a delayed check cannot excuse an early acquisition.
    for survivor in [1, 2] {
        replicas[survivor].renew_once(&pool).await;
        assert_no_early_takeover(&pool, &original, "before expiry").await;
    }
    // Replica 0 dies. The survivors keep trying; one takes over once the
    // database clock is past the expiry (never earlier). The attempt count is
    // only a retry budget for the takeover to happen eventually, not a bound on
    // how soon it must.
    wait_db_until(&pool, old_expires).await;
    let mut attempts = 0;
    let survivor = loop {
        attempts += 1;
        assert!(attempts <= 20, "no takeover");
        replicas[1].renew_once(&pool).await;
        assert_no_early_takeover(&pool, &original, "after expiry").await;
        replicas[2].renew_once(&pool).await;
        assert_no_early_takeover(&pool, &original, "after expiry").await;
        if let Some(i) = (1..3).find(|i| replicas[*i].held(Lease::Maintenance).is_some()) {
            break i;
        }
    };
    // The new term began no earlier than the old one's expiry, i.e. at least a
    // full term after the old one began (database time, no tolerance needed).
    let new_acquired: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT acquired_at FROM work_leases WHERE name='maintenance'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        new_acquired >= old_expires,
        "{new_acquired} < {old_expires}"
    );
    assert!(
        new_acquired - old_acquired >= term(),
        "{:?}",
        new_acquired - old_acquired
    );
    let new = replicas[survivor].held(Lease::Maintenance).unwrap();
    assert_eq!(new.epoch, old.epoch + 1);
    assert_ne!(new.holder, old.holder);
    // Its local view expired too (trusted until TTL - margin).
    assert!(replicas[0].held(Lease::Maintenance).is_none());
    // Fencing: the stale term cannot run job transactions or record runs.
    let error = crate::governance::rates::prune_fenced(&store, 10, Some(&old))
        .await
        .unwrap_err();
    assert!(is_fenced(&error), "{error:?}");
    assert!(!old.complete(&pool).await.unwrap());
    crate::governance::rates::prune_fenced(&store, 10, Some(&new))
        .await
        .unwrap();
    assert!(new.complete(&pool).await.unwrap());
    let recorded: Option<i64> =
        sqlx::query_scalar("SELECT last_completed_epoch FROM work_leases WHERE name='maintenance'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recorded, Some(new.epoch));
}

/// A job transaction holds its term: a takeover waits until the running job
/// commits, and the paused former leader cannot start another transaction.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_takeover_waits_for_the_running_job_transaction(pool: PgPool) {
    let replicas = holders(2);
    assert_eq!(replicas[0].renew_once(&pool).await, Lease::ALL.len());
    let fence = replicas[0].held(Lease::Lifecycle).unwrap();
    let mut job = pool.begin().await.unwrap();
    fence.check(&mut job).await.unwrap();
    // The term expires while the job transaction is still open.
    tokio::time::sleep(SHORT + Duration::from_millis(200)).await;
    let other = replicas[1].clone();
    let pool2 = pool.clone();
    let takeover = tokio::spawn(async move { other.renew_once(&pool2).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!takeover.is_finished(), "takeover must wait for the job");
    sqlx::query("SELECT 1").execute(&mut *job).await.unwrap();
    job.commit().await.unwrap();
    assert_eq!(takeover.await.unwrap(), Lease::ALL.len());
    assert_eq!(
        replicas[1].held(Lease::Lifecycle).unwrap().epoch,
        fence.epoch + 1
    );
    let mut late = pool.begin().await.unwrap();
    let error = fence.check(&mut late).await.unwrap_err();
    assert!(is_fenced(&error), "{error:?}");
}

/// Singleton ticks on three replicas: every tick runs exactly once in total,
/// including across a handover; release hands over at once.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn singleton_jobs_run_once_per_tick_across_three_replicas(pool: PgPool) {
    let replicas = holders(3);
    let stores = [
        replica(&pool).await,
        replica(&pool).await,
        replica(&pool).await,
    ];
    let runs = Arc::new(AtomicUsize::new(0));
    let mut stopped: Option<usize> = None;
    for tick in 0..6 {
        // A stopped replica no longer renews (its lease task ended).
        futures::future::join_all(
            replicas
                .iter()
                .zip(&stores)
                .enumerate()
                .filter(|(i, _)| Some(*i) != stopped)
                .map(|(_, (l, s))| l.renew_once(&s.pool)),
        )
        .await;
        if tick == 3 {
            // The current holder shuts down gracefully: immediate handover.
            let holder = replicas
                .iter()
                .position(|r| r.held(Lease::Lifecycle).is_some())
                .unwrap();
            replicas[holder].release_all(&stores[holder].pool).await;
            stopped = Some(holder);
            futures::future::join_all(
                replicas
                    .iter()
                    .zip(&stores)
                    .enumerate()
                    .filter(|(i, _)| *i != holder)
                    .map(|(_, (l, s))| l.renew_once(&s.pool)),
            )
            .await;
            assert!(replicas[holder].held(Lease::Lifecycle).is_none());
            assert!(replicas.iter().any(|r| r.held(Lease::Lifecycle).is_some()));
        }
        let before = runs.load(Ordering::SeqCst);
        futures::future::join_all(replicas.iter().zip(&stores).map(|(l, s)| {
            let runs = runs.clone();
            let store = s.clone();
            async move {
                run_singleton(
                    l,
                    Lease::Lifecycle,
                    "test",
                    Duration::from_secs(5),
                    |fence| async move {
                        let mut tx = store.pool.begin().await?;
                        fence.check(&mut tx).await?;
                        runs.fetch_add(1, Ordering::SeqCst);
                        tx.commit().await
                    },
                )
                .await
            }
        }))
        .await;
        assert_eq!(runs.load(Ordering::SeqCst) - before, 1, "tick {tick}");
    }
    let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM work_leases WHERE name='lifecycle'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(epoch, 2, "one handover");
}

/// The real lifecycle job on three replicas at once: a due account is
/// cleaned exactly once (one audit event), and nothing-due ticks take no
/// installation lock.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lifecycle_cleanup_runs_once_with_three_replicas(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    sqlx::query("UPDATE users SET disabled_at=now(),cleanup_due_at=now()-interval '1 minute',disable_reason='admin_suspension' WHERE id=$1")
        .bind(f.other)
        .execute(&pool)
        .await
        .unwrap();
    let replicas = holders(3);
    let stores = [
        replica(&pool).await,
        replica(&pool).await,
        replica(&pool).await,
    ];
    futures::future::join_all(
        replicas
            .iter()
            .zip(&stores)
            .map(|(l, s)| l.renew_once(&s.pool)),
    )
    .await;
    let cleaned: Vec<Option<u64>> =
        futures::future::join_all(replicas.iter().zip(&stores).map(|(l, s)| {
            let store = s.clone();
            async move {
                run_singleton(
                    l,
                    Lease::Lifecycle,
                    "lifecycle",
                    Duration::from_secs(10),
                    |fence| async move {
                        crate::lifecycle::cleanup_inactive_accounts_fenced(&store, Some(&fence))
                            .await
                    },
                )
                .await
            }
        }))
        .await;
    assert_eq!(cleaned.iter().flatten().copied().collect::<Vec<_>>(), [1]);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='user.cleaned' AND resource_id=$1",
    )
    .bind(f.other)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    // Nothing due: a cheap check, no lock and no fence needed.
    assert_eq!(
        crate::lifecycle::cleanup_inactive_accounts_fenced(&stores[0], None)
            .await
            .unwrap(),
        0
    );
}

/// The expired-lease queue on three replicas at once: every expired
/// reservation is reconciled exactly once (`FOR UPDATE SKIP LOCKED`), the
/// replicas share the work, and the totals stay exact.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn skip_locked_reconciliation_is_shared_without_duplicates(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let n = 150;
    for _ in 0..n {
        crate::governance::admit(&f.store, &f.start(), &request(), 1)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let stores = [
        replica(&pool).await,
        replica(&pool).await,
        replica(&pool).await,
    ];
    let counts: Vec<u64> = futures::future::join_all(stores.iter().map(|s| async move {
        let mut total = 0;
        loop {
            let n = crate::governance::reconcile_expired(s, 20).await.unwrap();
            total += n;
            if n == 0 {
                break total;
            }
        }
    }))
    .await;
    assert_eq!(counts.iter().sum::<u64>(), n, "{counts:?}");
    let (unknown, ledger): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM governance_reservations WHERE state='unknown'),(SELECT count(*) FROM monetary_ledger WHERE kind='unknown')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((unknown, ledger), (n as i64, n as i64));
    let mut tx = pool.begin().await.unwrap();
    let report = crate::governance::totals::verify_in(&mut tx).await.unwrap();
    assert!(report.consistent(), "{report:#?}");
}

/// The stored-file sweeper claims through `SKIP LOCKED` row leases: two
/// sweeps at once never claim the same file.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_file_sweeps_claim_disjoint_rows(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for _ in 0..40 {
        sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id,created_at) VALUES($1,'user_file/'||$2::text||'/'||$1::text,'user_file',$2,$3,'local','k1',now()-interval '2 days')")
            .bind(Uuid::new_v4())
            .bind(f.principal.workspace_id)
            .bind(f.principal.key_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let runtime = crate::filestore::FileStoreRuntime::off();
    let stores = [replica(&pool).await, replica(&pool).await];
    let reports = futures::future::join_all(
        stores
            .iter()
            .map(|s| crate::filestore::sweep::sweep_once(s, &runtime, 1000)),
    )
    .await;
    let claimed: u64 = reports.iter().map(|r| r.as_ref().unwrap().claimed).sum();
    assert_eq!(claimed, 40);
}

/// #106: a replica that can't do a lease's work never takes it, so the
/// replicas that can (audit export with a destination) always do.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_excluded_lease_goes_to_a_replica_that_can_do_the_work(pool: PgPool) {
    let (without, with) = (
        Arc::new(Leases::with_ttl(SHORT).without(Lease::AuditExport)),
        Arc::new(Leases::with_ttl(SHORT)),
    );
    let (a, b) = (replica(&pool).await, replica(&pool).await);
    // The excluded replica renews first and still never takes it.
    without.renew_once(&a.pool).await;
    with.renew_once(&b.pool).await;
    assert!(without.held(Lease::AuditExport).is_none());
    assert!(with.held(Lease::AuditExport).is_some());
    assert_eq!(
        Lease::ALL
            .iter()
            .filter(|l| without.held(**l).is_some())
            .count()
            + Lease::ALL
                .iter()
                .filter(|l| with.held(**l).is_some())
                .count(),
        Lease::ALL.len(),
        "every lease still has exactly one holder"
    );
}
