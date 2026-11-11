//! #389 on real PostgreSQL: a failed route lookup or admission names its
//! stage. (The settlement side is `inference::steering_tests::settlement_fault_*`.)
use super::*;
use crate::db::accounting_failure::{Cause, Stage};
use crate::db::diagnostics::{self, Handle, Signal};
use crate::governance::tests::db::{fixture, request};
use sqlx::{Connection, PgPool};
use std::time::Duration;

/// A store with its own pool of `connections` on the test database, each
/// connection set up with `setup`.
async fn store_with(pool: &PgPool, connections: u32, setup: &'static str) -> Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(connections)
        .acquire_timeout(Duration::from_millis(500))
        .after_connect(move |connection, _| {
            Box::pin(async move {
                sqlx::query(setup).execute(connection).await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    Store::new(pool)
}

async fn admission(
    store: &Store,
    start: &ExecutionStart,
) -> (Result<(), InferenceError>, Vec<Signal>) {
    let capture = Handle::new();
    let result = diagnostics::scope(capture.clone(), admit(store, start, &request(), 30)).await;
    (result, capture.signals())
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn admission_without_a_pooled_connection_is_recorded_as_a_failed_start(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1).await;
    let store = store_with(&pool, 1, "SELECT 1").await;
    let (admitted, signals) = admission(&store, &f.start()).await;
    assert!(admitted.is_ok(), "the unfaulted admission succeeds");
    assert!(signals.is_empty(), "nothing is recorded on success");

    let held = store.pool.acquire().await.unwrap();
    let (refused, signals) = admission(&store, &f.start()).await;
    drop(held);
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(signals, [(Stage::PoolAcquireAndBegin, Cause::PoolTimeout)]);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_lock_timeout_inside_admission_is_recorded_as_a_failed_statement(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1).await;
    let store = store_with(&pool, 2, "SET lock_timeout = '500ms'").await;
    let (admitted, signals) = admission(&store, &f.start()).await;
    assert!(admitted.is_ok(), "the unfaulted admission succeeds");
    assert!(signals.is_empty(), "nothing is recorded on success");

    // Another session holds the table every admission writes its hold to.
    let mut blocker = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::raw_sql("BEGIN; LOCK TABLE governance_reservations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut blocker)
        .await
        .unwrap();
    let (refused, signals) = admission(&store, &f.start()).await;
    sqlx::query("ROLLBACK").execute(&mut blocker).await.unwrap();
    blocker.close().await.unwrap();
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(signals, [(Stage::GovernanceStorage, Cause::LockTimeout)]);
}

/// The first database reads of a request: with no pooled connection the
/// lookup of the eligible deployments and the routing plan name their stage.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_route_lookup_without_a_pooled_connection_is_recorded_at_its_stage(pool: PgPool) {
    use crate::inference::repository::InferenceRepository;
    let f = fixture(pool.clone()).await;
    let store = store_with(&pool, 1, "SELECT 1").await;
    let capture = Handle::new();
    let candidates = diagnostics::scope(capture.clone(), async {
        let candidates = store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1, "the fixture's deployment is eligible");
        store
            .route_plan(&f.principal, "company/smart", &candidates, Uuid::new_v4())
            .await
            .unwrap();
        candidates
    })
    .await;
    assert!(
        capture.signals().is_empty(),
        "nothing is recorded on success"
    );

    let held = store.pool.acquire().await.unwrap();
    let capture = Handle::new();
    diagnostics::scope(capture.clone(), async {
        assert!(matches!(
            store.deployments(&f.principal, "company/smart").await,
            Err(InferenceError::Storage)
        ));
        assert!(matches!(
            store
                .route_plan(&f.principal, "company/smart", &candidates, Uuid::new_v4())
                .await,
            Err(InferenceError::Storage)
        ));
    })
    .await;
    drop(held);
    assert_eq!(
        capture.signals(),
        [
            (Stage::RouteLookup, Cause::PoolTimeout),
            (Stage::RouteLookup, Cause::PoolTimeout),
        ]
    );
}
