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

/// Another session that holds `table` exclusively until [`unlock`].
async fn lock_table(pool: &PgPool, table: &'static str) -> sqlx::PgConnection {
    let mut blocker = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::raw_sql(&format!(
        "BEGIN; LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"
    ))
    .execute(&mut blocker)
    .await
    .unwrap();
    blocker
}

async fn unlock(mut blocker: sqlx::PgConnection) {
    sqlx::query("ROLLBACK").execute(&mut blocker).await.unwrap();
    blocker.close().await.unwrap();
}

/// The lookup of the eligible deployments while another session holds `table`.
async fn deployments_under_lock(
    pool: &PgPool,
    store: &Store,
    principal: &crate::auth::Principal,
    table: &'static str,
) -> (
    Result<Vec<crate::inference::types::Deployment>, InferenceError>,
    Vec<Signal>,
) {
    use crate::inference::repository::InferenceRepository;
    let blocker = lock_table(pool, table).await;
    let capture = Handle::new();
    let result = diagnostics::scope(
        capture.clone(),
        store.deployments(principal, "company/smart"),
    )
    .await;
    unlock(blocker).await;
    (result, capture.signals())
}

/// The later statements of the deployment lookup (after the transaction
/// start): each failure is one `route_lookup` signal.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_failed_later_statement_of_the_deployment_lookup_is_recorded_once(pool: PgPool) {
    use crate::inference::repository::InferenceRepository;
    let f = fixture(pool.clone()).await;
    let store = store_with(&pool, 2, "SET lock_timeout = '200ms'").await;
    let lookup = || async {
        let capture = Handle::new();
        let result = diagnostics::scope(
            capture.clone(),
            store.deployments(&f.principal, "company/smart"),
        )
        .await;
        (result, capture.signals())
    };
    let (found, signals) = lookup().await;
    assert_eq!(found.unwrap().len(), 1, "the unfaulted lookup succeeds");
    assert!(signals.is_empty(), "nothing is recorded on success");

    // The revalidation of the key reads `api_keys`; the transaction start does not.
    let (refused, signals) = deployments_under_lock(&pool, &store, &f.principal, "api_keys").await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::LockTimeout)],
        "the revalidation"
    );

    // Only the deployments query reads `provider_connections`.
    let (refused, signals) =
        deployments_under_lock(&pool, &store, &f.principal, "provider_connections").await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::LockTimeout)],
        "the deployments query"
    );

    // COMMIT: the deployments query now queues a deferred constraint trigger
    // (through the eligibility function it calls) that raises at commit.
    sqlx::raw_sql(
        r#"CREATE TABLE commit_fault_389(id int);
           CREATE FUNCTION commit_fault_389() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN RAISE EXCEPTION 'commit fault'; END $$;
           CREATE CONSTRAINT TRIGGER commit_fault_389 AFTER INSERT ON commit_fault_389
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION commit_fault_389();
           ALTER FUNCTION workspace_model_allowed(uuid,uuid) RENAME TO workspace_model_allowed_389;
           CREATE FUNCTION workspace_model_allowed(ws uuid, model uuid) RETURNS boolean
             LANGUAGE plpgsql VOLATILE AS
             $$ BEGIN INSERT INTO commit_fault_389 VALUES (1);
                RETURN workspace_model_allowed_389(ws, model); END $$"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let (refused, signals) = lookup().await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::OtherDatabase)],
        "the commit"
    );
}

/// The routing plan of `candidates` while another session holds `table`.
async fn plan_under_lock(
    pool: &PgPool,
    store: &Store,
    principal: &crate::auth::Principal,
    candidates: &[crate::inference::types::Deployment],
    table: &'static str,
) -> (
    Result<crate::routing::RoutePlan, InferenceError>,
    Vec<Signal>,
) {
    use crate::inference::repository::InferenceRepository;
    let blocker = lock_table(pool, table).await;
    let capture = Handle::new();
    let result = diagnostics::scope(
        capture.clone(),
        store.route_plan(principal, "company/smart", candidates, Uuid::new_v4()),
    )
    .await;
    unlock(blocker).await;
    (result, capture.signals())
}

/// The second statement of the live routing plan (the candidates' routing
/// rows and health): its failure is one `route_lookup` signal.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_failed_later_statement_of_the_live_routing_plan_is_recorded_once(pool: PgPool) {
    use crate::inference::repository::InferenceRepository;
    let f = fixture(pool.clone()).await;
    let store = store_with(&pool, 2, "SET lock_timeout = '200ms'").await;
    assert!(!store.caches.enabled(), "this store plans live");
    let candidates = store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap();
    // The policy statement does not read `deployment_routing`; the rows statement does.
    let (refused, signals) = plan_under_lock(
        &pool,
        &store,
        &f.principal,
        &candidates,
        "deployment_routing",
    )
    .await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::LockTimeout)],
        "the live routing rows"
    );
}

/// The three statements of the cached routing plan (policy, routing rows,
/// passive health), each on a replica whose caches are still empty: each
/// failure is one `route_lookup` signal.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_failed_statement_of_the_cached_routing_plan_is_recorded_once(pool: PgPool) {
    use crate::inference::repository::InferenceRepository;
    let f = fixture(pool.clone()).await;
    let candidates = store_with(&pool, 2, "SELECT 1")
        .await
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1, "the fixture's deployment is eligible");
    // A replica with caching on, its versions confirmed, and nothing cached yet.
    let cached = || async {
        let store = store_with(&pool, 2, "SET lock_timeout = '200ms'").await;
        store.caches.versions.enable();
        crate::notify::poll_once(&store.pool, &store.caches.versions)
            .await
            .unwrap();
        store.caches.versions.pin_sync();
        assert!(
            store.caches.enabled(),
            "this store plans through the caches"
        );
        store
    };
    let plan = |store: Store| {
        let (principal, candidates) = (f.principal, candidates.clone());
        async move {
            let capture = Handle::new();
            let result = diagnostics::scope(
                capture.clone(),
                store.route_plan(&principal, "company/smart", &candidates, Uuid::new_v4()),
            )
            .await;
            (result, capture.signals())
        }
    };

    // The first statement, the policy, is the only reader of `routing_policies`.
    let store = cached().await;
    let (refused, signals) =
        plan_under_lock(&pool, &store, &f.principal, &candidates, "routing_policies").await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::LockTimeout)],
        "the cached policy"
    );

    // The policy statement does not read `deployment_routing`; the rows statement does.
    let store = cached().await;
    let (refused, signals) = plan_under_lock(
        &pool,
        &store,
        &f.principal,
        &candidates,
        "deployment_routing",
    )
    .await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::LockTimeout)],
        "the cached routing rows"
    );

    // Only the health statement reads `deployment_health` on this path.
    let store = cached().await;
    let (refused, signals) = plan_under_lock(
        &pool,
        &store,
        &f.principal,
        &candidates,
        "deployment_health",
    )
    .await;
    assert!(matches!(refused, Err(InferenceError::Storage)));
    assert_eq!(
        signals,
        [(Stage::RouteLookup, Cause::LockTimeout)],
        "the cached health"
    );

    // Unfaulted, the same replica plans through its caches and records nothing.
    let (planned, signals) = plan(store).await;
    assert_eq!(planned.unwrap().deployment_ids, [f.deployment]);
    assert!(signals.is_empty(), "nothing is recorded on success");
}

/// One settlement under `deadline`; the signals of its elapsed deadline.
/// `rerun` first leaves the mark of an earlier run of the same settlement (a
/// deadlock victim is re-run inside the same deadline and queues again).
async fn elapsed_settlement(
    store: &Store,
    record: &ExecutionFinish,
    deadline: Duration,
    rerun: bool,
) -> Vec<Signal> {
    use crate::db::accounting_failure::{Waiting, explicit_settlement, settlement_waiting};
    let capture = Handle::new();
    let settled = diagnostics::scope(
        capture.clone(),
        explicit_settlement(tokio::time::timeout(deadline, async {
            if rerun {
                settlement_waiting(Waiting::Database);
            }
            finish(store, record).await
        })),
    )
    .await;
    assert!(settled.is_err(), "the settlement deadline elapsed");
    capture.signals()
}

/// What an elapsed settlement deadline was waiting on: the in-process
/// settlement gate (every permit held), or, past the gate, a pooled connection.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_elapsed_settlement_deadline_names_the_gate_or_the_connection_wait(pool: PgPool) {
    use crate::governance::tests::db::done;
    let f = fixture(pool.clone()).await;
    f.price(1).await;
    // An acquire timeout (500 ms) longer than the deadline used here, as the
    // production settlement deadline starts before the equal acquire timeout.
    let store = store_with(&pool, 2, "SELECT 1").await;
    let deadline = Duration::from_millis(250);
    let start = f.start();
    admit(&store, &start, &request(), 30).await.unwrap();
    let record = done(start.id, Some(4), Some(2));

    // Every settlement permit is taken: the settlement waits at the gate,
    // with the whole pool idle.
    let gate = &store.lock_gates.settlement;
    let permits = gate.acquire_many(gate.available_permits() as u32).await;
    assert_eq!(gate.available_permits(), 0);
    assert_eq!(
        elapsed_settlement(&store, &record, deadline, false).await,
        [(Stage::ExplicitSettlement, Cause::DeadlineInQueue)],
        "a settlement waiting at the gate"
    );
    assert_eq!(
        elapsed_settlement(&store, &record, deadline, true).await,
        [(Stage::ExplicitSettlement, Cause::DeadlineInQueue)],
        "a re-run settlement waiting at the gate again"
    );
    drop(permits);

    // Past the gate with every pooled connection busy.
    let held = [
        store.pool.acquire().await.unwrap(),
        store.pool.acquire().await.unwrap(),
    ];
    assert_eq!(
        elapsed_settlement(&store, &record, deadline, false).await,
        [(
            Stage::ExplicitSettlement,
            Cause::DeadlineAcquiringConnection
        )],
        "a settlement past the gate waiting for a connection"
    );
    drop(held);

    // Unfaulted, the same settlement completes and records nothing.
    let capture = Handle::new();
    diagnostics::scope(capture.clone(), finish(&store, &record))
        .await
        .unwrap();
    assert!(
        capture.signals().is_empty(),
        "nothing is recorded on success"
    );
}
