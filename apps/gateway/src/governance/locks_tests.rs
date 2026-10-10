//! Scoped admission (scale plan P3, migration 0027) on real PostgreSQL:
//! parallelism across scopes, exact budgets under concurrency, revocation,
//! membership, suspension, type-default and catalog changes racing admission,
//! settlement versus reconciliation, and a deadlock-freedom fuzz of random
//! admissions, settlements and management changes. Every test runs under the
//! selected `GATEWAY_ADMISSION_MODE` (the suite runs both); assertions that
//! differ between the modes say so.
use super::locks::AdmissionMode;
use super::*;
use crate::{
    auth::{NewApiKey, Principal},
    governance::tests::db::{Fixture, done, fixture, request},
    inference::error::LimitScope,
};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use sqlx::PgPool;
use std::time::Duration;

/// The wide-pool tests run one at a time (the test server allows 100
/// connections and the rest of the suite runs in parallel).
static WIDE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// A store on the test database with a wide pool, so admissions really run
/// on many connections at once.
async fn wide(pool: &PgPool, connections: u32) -> Store {
    let connections = connections.min(24);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(connections)
        .acquire_timeout(Duration::from_secs(120))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    Store::new(pool)
}

fn start_for(f: &Fixture, principal: Principal) -> ExecutionStart {
    ExecutionStart {
        principal,
        ..f.start()
    }
}

/// A key for `user` in workspace `ws` (a member's own key).
async fn key_for(pool: &PgPool, ws: Uuid, user: Uuid) -> Principal {
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'race',$4)")
        .bind(key.id).bind(ws).bind(user).bind(key.digest.as_slice()).execute(pool).await.unwrap();
    Principal {
        key_id: key.id,
        workspace_id: ws,
        user_id: Some(user),
    }
}

async fn verify_clean(pool: &PgPool, context: &str) {
    let mut tx = pool.begin().await.unwrap();
    let report = totals::verify_in(&mut tx).await.unwrap();
    assert!(report.consistent(), "{context}: {report:#?}");
}

fn scoped(store: &Store) -> bool {
    store.admission_mode() == AdmissionMode::Scoped
}

/// Hold workspace `p`'s admission locks and rows (scoped) or the installation
/// row (global) in an open transaction, as a long admission would.
async fn hold_admission(
    pool: &PgPool,
    mode: AdmissionMode,
    p: &Principal,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = pool.begin().await.unwrap();
    match mode {
        AdmissionMode::Scoped => {
            sqlx::query("SELECT * FROM omg_admission_locks($1,$2,$3)")
                .bind(p.workspace_id)
                .bind(p.user_id)
                .bind(p.key_id)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("SELECT omg_lock_scope_rows($1,$2,ARRAY[clock_timestamp()],true)")
                .bind(vec![p.workspace_id])
                .bind(vec![p.key_id])
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        AdmissionMode::Global => {
            sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("SELECT lock_installation()")
                .execute(&mut *tx)
                .await
                .unwrap();
        }
    }
    tx
}

/// Two workspaces admit in parallel: while one workspace's admission holds
/// its scope locks and rows, another workspace admits immediately; the same
/// workspace waits. (Global mode: everything waits on the installation row.)
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn two_workspaces_admit_in_parallel_without_waiting(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    f.policy(
        "workspace_local_policies",
        Some(1000),
        Some(1_000_000),
        Some(100),
        Some(1_000_000),
    )
    .await;
    let mode = f.store.admission_mode();
    let held = hold_admission(&pool, mode, &f.principal).await;
    let req = request();
    // Another workspace (the team) is not blocked in scoped mode.
    let other = start_for(&f, f.team);
    let other_admission = admit(&f.store, &other, &req, 30);
    tokio::pin!(other_admission);
    let first = tokio::time::timeout(Duration::from_millis(500), &mut other_admission).await;
    // The held workspace waits in both modes.
    let same = f.start();
    let same_admission = admit(&f.store, &same, &req, 30);
    tokio::pin!(same_admission);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut same_admission)
            .await
            .is_err(),
        "an admission of the held workspace must wait"
    );
    match mode {
        AdmissionMode::Scoped => assert_eq!(first.expect("other workspace waited"), Ok(())),
        AdmissionMode::Global => assert!(first.is_err(), "global mode serializes"),
    }
    held.commit().await.unwrap();
    if mode == AdmissionMode::Global {
        let (a, b) = tokio::time::timeout(
            Duration::from_secs(5),
            futures::future::join(&mut same_admission, &mut other_admission),
        )
        .await
        .unwrap();
        a.unwrap();
        b.unwrap();
    } else {
        tokio::time::timeout(Duration::from_secs(5), &mut same_admission)
            .await
            .unwrap()
            .unwrap();
    }
    verify_clean(&pool, "parallel").await;
}

/// Many concurrent admissions on many connections against one workspace
/// budget and one key budget: exactly the holds that fit are admitted, the
/// rest are exact budget denials, and the totals stay equal to the scan.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn same_workspace_budget_never_overspent_under_concurrent_admission(pool: PgPool) {
    let _wide = WIDE.acquire().await.unwrap();
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let hold = 110; // 100 input + 10 output tokens at 1 USD/M
    let fit = 17;
    set_test_budget(
        &pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        Some(hold * fit),
    )
    .await;
    // The team workspace: a key budget (lineage scope) instead.
    let team_lineage: Uuid =
        sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE id=$1")
            .bind(f.team.key_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    set_test_budget(
        &pool,
        "key",
        None,
        Some(f.team.workspace_id),
        Some(team_lineage),
        "day",
        Some(hold * fit),
    )
    .await;
    let store = wide(&pool, 48).await;
    let mut tasks = Vec::new();
    for i in 0..(fit as usize * 4) {
        for principal in [f.principal, f.team] {
            let (store, s) = (store.clone(), start_for(&f, principal));
            tasks.push(tokio::spawn(async move {
                let result = admit(&store, &s, &request(), 60).await;
                // Settle a third while others still admit (frees nothing:
                // actual 110 = hold, but exercises settlement concurrency).
                if result.is_ok() && i % 3 == 0 {
                    finish(&store, &done(s.id, Some(100), Some(10)))
                        .await
                        .unwrap();
                }
                (principal.workspace_id, result)
            }));
        }
    }
    let (mut personal, mut team) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            (ws, Ok(())) if ws == f.principal.workspace_id => personal += 1,
            (_, Ok(())) => team += 1,
            (ws, Err(InferenceError::BudgetExceeded(scope))) => assert_eq!(
                scope,
                if ws == f.principal.workspace_id {
                    LimitScope::Workspace
                } else {
                    LimitScope::ApiKey
                }
            ),
            (_, Err(e)) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!((personal, team), (fit, fit), "exactly the holds that fit");
    let used: Vec<i64> = sqlx::query_scalar("SELECT (settled_microusd+held_microusd)::bigint FROM budget_totals WHERE (scope_kind='workspace' AND scope_id=$1 AND period='month') OR (scope_kind='key' AND scope_id=$2 AND period='day') ORDER BY scope_kind")
        .bind(f.principal.workspace_id).bind(team_lineage).fetch_all(&pool).await.unwrap();
    assert_eq!(used, vec![hold * fit, hold * fit]);
    verify_clean(&pool, "budget").await;
}

/// Spawn `n` staggered admissions of `principal`; at `after`, run `mutate`
/// (a management-style transaction: catalog lock, installation row, then the
/// change, whose triggers take the scope locks) and return the database time
/// read after its locks were acquired. Admissions that succeeded must have
/// been admitted before that instant; every admission started after the
/// change committed must fail with `expected`; admitted ones still settle.
async fn race<F, Fut>(f: &Fixture, principal: Principal, mutate: F, expected: InferenceError)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: std::future::Future<Output = DateTime<Utc>>,
{
    let _wide = WIDE.acquire().await.unwrap();
    let pool = f.store.pool.clone();
    let store = wide(&pool, 32).await;
    let mut tasks = Vec::new();
    for i in 0..60u64 {
        let (store, s) = (store.clone(), start_for(f, principal));
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(i)).await;
            let result = admit(&store, &s, &request(), 60).await;
            (s.id, result)
        }));
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    let changed_at = mutate(pool.clone()).await;
    let mut admitted = Vec::new();
    for t in tasks {
        match t.await.unwrap() {
            (id, Ok(())) => admitted.push(id),
            (_, Err(e)) => assert_eq!(e, expected),
        }
    }
    eprintln!("race: {} of 60 admitted before the change", admitted.len());
    for id in &admitted {
        let at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT admitted_at FROM governance_reservations WHERE execution_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            at < changed_at,
            "admitted at {at} after the change locked at {changed_at}"
        );
        // In-flight work admitted before the change still settles.
        finish(&store, &done(*id, Some(100), Some(10)))
            .await
            .unwrap();
    }
    for _ in 0..5 {
        assert_eq!(
            admit(&store, &start_for(f, principal), &request(), 60).await,
            Err(expected)
        );
    }
    verify_clean(&pool, "race").await;
}

/// Run `sql` (returning `clock_timestamp()`) like a management change:
/// shared catalog lock (exclusive with `catalog`), installation row, change.
/// No explicit scope locks: the 0027 triggers take them.
async fn management(pool: PgPool, catalog: bool, sql: &'static str, id: Uuid) -> DateTime<Utc> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(if catalog {
        "SELECT pg_advisory_xact_lock(72419502)"
    } else {
        "SELECT pg_advisory_xact_lock_shared(72419502)"
    })
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("SELECT lock_installation()")
        .execute(&mut *tx)
        .await
        .unwrap();
    let at: DateTime<Utc> = sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    at
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revoked_key_is_never_admitted_after_revocation_commits(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let key = f.principal.key_id;
    race(
        &f,
        f.principal,
        |pool| {
            management(
                pool,
                false,
                "UPDATE api_keys SET revoked_at=now() WHERE id=$1 RETURNING clock_timestamp()",
                key,
            )
        },
        InferenceError::Unauthenticated,
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn membership_removal_races_admission(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let member = key_for(&pool, f.team.workspace_id, f.other).await;
    let other = f.other;
    race(&f, member, |pool| management(pool, false, "UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL RETURNING clock_timestamp()", other), InferenceError::Unauthenticated).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn user_suspension_races_admission(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let member = key_for(&pool, f.team.workspace_id, f.other).await;
    let other = f.other;
    race(
        &f,
        member,
        |pool| {
            management(
                pool,
                false,
                "UPDATE users SET disabled_at=now() WHERE id=$1 RETURNING clock_timestamp()",
                other,
            )
        },
        InferenceError::Unauthenticated,
    )
    .await;
}

/// A workspace-type default budget (one change affecting every personal
/// workspace) racing admission: the type-scope lock admission takes shared.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn type_default_limit_change_races_admission(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    race(&f, f.principal, |pool| management(pool, false, "INSERT INTO policy_budgets(layer,kind,period,amount_microusd) SELECT 'type','personal','month',0 WHERE $1::uuid IS NOT NULL RETURNING clock_timestamp()", Uuid::nil()), InferenceError::BudgetExceeded(LimitScope::Workspace)).await;
}

/// A workspace override (platform per-workspace replacement) racing admission.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspace_override_change_races_admission(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let ws = f.principal.workspace_id;
    race(&f, f.principal, |pool| management(pool, false, "WITH o AS (INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1) RETURNING workspace_id) INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) SELECT 'override',workspace_id,'day',0 FROM o RETURNING clock_timestamp()", ws), InferenceError::BudgetExceeded(LimitScope::Workspace)).await;
}

/// A global catalog change (route disabled under the exclusive catalog lock).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn catalog_change_races_admission(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let d = f.deployment;
    race(
        &f,
        f.principal,
        |pool| {
            management(
                pool,
                true,
                "UPDATE deployments SET enabled=false WHERE id=$1 RETURNING clock_timestamp()",
                d,
            )
        },
        InferenceError::ModelUnavailable,
    )
    .await;
}

/// Finish and lease reconciliation racing on the same expired reservations:
/// each ends exactly once (settled by the finish, or unknown with its hold
/// retained by reconciliation, after which the finish fails), with exactly
/// one terminal ledger row, and the totals stay exact.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn settlement_and_reconciliation_race_exactly_once(pool: PgPool) {
    let _wide = WIDE.acquire().await.unwrap();
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    // Admit in the past so every lease has already expired.
    let past = wide(&pool, 8).await;
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()-interval '10 minutes'")
        .fetch_one(&pool)
        .await
        .unwrap();
    past.pin_admission_clock(at).unwrap();
    let mut ids = Vec::new();
    for i in 0..40 {
        let s = start_for(&f, if i % 2 == 0 { f.principal } else { f.team });
        admit(&past, &s, &request(), 1).await.unwrap();
        ids.push(s.id);
    }
    let store = wide(&pool, 32).await;
    let mut tasks = Vec::new();
    for id in ids.clone() {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            finish(&store, &done(id, Some(50), Some(5))).await.is_ok()
        }));
    }
    for _ in 0..4 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            reconcile_expired(&store, 1000).await.unwrap();
            false
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    reconcile_expired(&store, 1000).await.unwrap();
    let rows: Vec<(String, i64, Option<i64>, Option<i64>)> = sqlx::query_as("SELECT r.state,(SELECT count(*) FROM monetary_ledger l WHERE l.execution_id=r.execution_id AND l.kind<>'hold'),r.held_microusd,r.actual_microusd FROM governance_reservations r WHERE r.execution_id=ANY($1)")
        .bind(&ids).fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), ids.len());
    for (state, terminal, held, actual) in rows {
        assert_eq!(terminal, 1, "exactly one terminal ledger row");
        match state.as_str() {
            "settled" => assert_eq!(actual, Some(55)),
            "unknown" => assert_eq!((held, actual), (Some(110), None), "unknown keeps its hold"),
            other => panic!("{other}"),
        }
    }
    verify_clean(&pool, "settle vs reconcile").await;
}

/// Random interleavings of admissions (three workspaces, five keys),
/// settlements, failures, reconciliation and management changes of every
/// authority scope (explicit canonical scope locks, trigger-only changes,
/// type defaults and exclusive catalog changes) on many connections: no
/// deadlock is ever detected (`pg_stat_database.deadlocks` stays 0, so the
/// 40P01 retry was never needed), no budget is overspent and the totals and
/// counters equal the full scan.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deadlock_freedom_fuzz(pool: PgPool) {
    let _wide = WIDE.acquire().await.unwrap();
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let member = key_for(&pool, f.team.workspace_id, f.other).await;
    let second = key_for(&pool, f.team.workspace_id, f.owner).await;
    // A spare route that catalog changes toggle (the served one stays up).
    let spare = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,model_id,provider_connection_id,'spare',false FROM deployments WHERE id=$2")
        .bind(spare).bind(f.deployment).execute(&pool).await.unwrap();
    let budget = 110 * 40;
    set_test_budget(
        &pool,
        "local",
        None,
        Some(f.team.workspace_id),
        None,
        "day",
        Some(budget),
    )
    .await;
    sqlx::query("INSERT INTO workspace_type_policies(kind,requests_per_minute,concurrent_requests) VALUES('personal',100000,64),('team',100000,64) ON CONFLICT(kind) DO UPDATE SET requests_per_minute=EXCLUDED.requests_per_minute,concurrent_requests=EXCLUDED.concurrent_requests")
        .execute(&pool).await.unwrap();
    let principals = [f.principal, f.team, member, second];
    let store = wide(&pool, 40).await;
    let mut tasks = Vec::new();
    for worker in 0..24u64 {
        let store = store.clone();
        let starts: Vec<ExecutionStart> = (0..12)
            .map(|i| start_for(&f, principals[(worker as usize + i) % 4]))
            .collect();
        tasks.push(tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(worker);
            for s in starts {
                match admit(
                    &store,
                    &s,
                    &request(),
                    if rng.random_bool(0.2) { 1 } else { 60 },
                )
                .await
                {
                    Ok(()) => {
                        if rng.random_bool(0.7) {
                            let record = if rng.random_bool(0.2) {
                                ExecutionFinish {
                                    id: s.id,
                                    outcome: Outcome::Failed,
                                    error: Some(InferenceError::UpstreamUnavailable),
                                    usage: Usage::default(),
                                    elapsed_ms: 1,
                                }
                            } else {
                                done(
                                    s.id,
                                    Some(rng.random_range(1..=100)),
                                    Some(rng.random_range(1..=10)),
                                )
                            };
                            finish(&store, &record).await.unwrap();
                        }
                    }
                    Err(
                        InferenceError::BudgetExceeded(_)
                        | InferenceError::Busy
                        | InferenceError::Unauthenticated
                        | InferenceError::ModelUnavailable,
                    ) => {}
                    Err(e) => panic!("{e:?}"),
                }
                if rng.random_bool(0.1) {
                    reconcile_expired(&store, 100).await.unwrap();
                }
            }
        }));
    }
    // Management writers: random scope changes in canonical order with
    // explicit locks (as the gateway does), and trigger-only changes.
    let (ws_p, ws_t) = (f.principal.workspace_id, f.team.workspace_id);
    let (owner, other) = (f.owner, f.other);
    let lineages: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT governance_key_id FROM api_keys")
        .fetch_all(&pool)
        .await
        .unwrap();
    for worker in 0..6u64 {
        let pool = store.pool.clone();
        let lineages = lineages.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(1000 + worker);
            for _ in 0..25 {
                let mut tx = pool.begin().await.unwrap();
                let catalog = rng.random_bool(0.1);
                sqlx::query(if catalog { "SELECT pg_advisory_xact_lock(72419502)" } else { "SELECT pg_advisory_xact_lock_shared(72419502)" })
                    .execute(&mut *tx).await.unwrap();
                sqlx::query("SELECT lock_installation()").execute(&mut *tx).await.unwrap();
                let lineage = lineages[rng.random_range(0..lineages.len())];
                let op = rng.random_range(0..7);
                // Explicit: every scope the change touches, up front, in
                // canonical order (as the gateway does); otherwise the
                // triggers take them lazily (one change per transaction).
                if rng.random_bool(0.5) {
                    let scopes: Vec<locks::Scope> = match op {
                        0 | 1 => vec![locks::Scope::Lineage(lineage)],
                        2 => vec![locks::Scope::User(other)],
                        3 => vec![locks::Scope::Type(locks::WorkspaceType::Team)],
                        4 => vec![locks::Scope::Workspace(ws_p)],
                        5 => vec![locks::Scope::User(owner), locks::Scope::User(other)],
                        _ => vec![],
                    };
                    locks::exclusive(&mut tx, scopes).await.unwrap();
                }
                match op {
                    0 => { sqlx::query("UPDATE api_keys SET disabled_at=CASE WHEN disabled_at IS NULL THEN now() END WHERE governance_key_id=$1").bind(lineage).execute(&mut *tx).await.unwrap(); }
                    1 => { sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,concurrent_requests) SELECT workspace_id,$1,50 FROM api_keys WHERE id=$1 ON CONFLICT(workspace_id,governance_key_id) DO UPDATE SET concurrent_requests=CASE WHEN key_policies.concurrent_requests IS NULL THEN 50 END").bind(lineage).execute(&mut *tx).await.unwrap(); }
                    2 => { sqlx::query("UPDATE workspace_membership_grants SET revoked_at=CASE WHEN revoked_at IS NULL THEN now() END WHERE user_id=$1 AND workspace_id=$2").bind(other).bind(ws_t).execute(&mut *tx).await.unwrap(); }
                    3 => { sqlx::query("UPDATE workspace_type_policies SET concurrent_requests=CASE WHEN concurrent_requests=64 THEN 63 ELSE 64 END WHERE kind='team'").execute(&mut *tx).await.unwrap(); }
                    4 => { sqlx::query("INSERT INTO workspace_local_policies(workspace_id,requests_per_minute) VALUES($1,99999) ON CONFLICT(workspace_id) DO UPDATE SET requests_per_minute=CASE WHEN workspace_local_policies.requests_per_minute=99999 THEN 99998 ELSE 99999 END").bind(ws_p).execute(&mut *tx).await.unwrap(); }
                    5 => { sqlx::query("UPDATE users SET disabled_at=CASE WHEN disabled_at IS NULL AND id=$2 THEN now() END WHERE id IN ($1,$2)").bind(owner).bind(other).execute(&mut *tx).await.unwrap(); }
                    _ if catalog => { sqlx::query("UPDATE deployments SET enabled=NOT enabled WHERE id=$1").bind(spare).execute(&mut *tx).await.unwrap(); }
                    _ => {}
                }
                tx.commit().await.unwrap();
                tokio::time::sleep(Duration::from_millis(rng.random_range(0..4))).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    reconcile_expired(&store, 10_000).await.ok();
    let used: Option<i64> = sqlx::query_scalar("SELECT (settled_microusd+held_microusd)::bigint FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'")
        .bind(f.team.workspace_id).fetch_optional(&pool).await.unwrap();
    // Settlement may exceed holds (observed usage), never admission; the
    // fuzz settles at or below the hold, so the budget bounds the total.
    assert!(used.unwrap_or(0) <= budget, "{used:?} > {budget}");
    verify_clean(&pool, "fuzz").await;
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    store.pool.close().await;
    // Closed backends flush their statistics on exit.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let deadlocks: i64 =
        sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname=$1")
            .bind(&db)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deadlocks, 0, "a deadlock was detected");
}

/// The `omg_scope_key` SQL helper and the gateway agree on lock keys, and
/// audit mode refuses out-of-order scope locks and authority changes
/// without their up-front lock (the lane F audit used by the whole suite).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lock_keys_match_and_audit_refuses_out_of_order_locks(pool: PgPool) {
    let mut rng = StdRng::seed_from_u64(7);
    for _ in 0..200 {
        let id = Uuid::from_u128(rng.random());
        let key: i32 = sqlx::query_scalar("SELECT omg_scope_key($1)")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(key, locks::Scope::Workspace(id).key());
    }
    let f = fixture(pool.clone()).await;
    let lineage = f.principal.key_id;
    // Out of order: a lineage lock, then a workspace lock.
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::Lineage(lineage)])
        .await
        .unwrap();
    assert!(
        locks::exclusive(&mut tx, [locks::Scope::Workspace(f.principal.workspace_id)])
            .await
            .is_err()
            == audit_on()
    );
    drop(tx);
    // A gateway transaction (catalog lock held) changing a key without its lock.
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let refused = sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id=$1")
        .bind(f.principal.key_id)
        .execute(&mut *tx)
        .await
        .is_err();
    assert_eq!(refused, audit_on());
    drop(tx);
    // With the lock taken up front it passes.
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await
        .unwrap();
    locks::exclusive(&mut tx, [locks::Scope::Lineage(lineage)])
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id=$1")
        .bind(f.principal.key_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Exclusive catalog lock: every admission is excluded, no audit.
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(72419502)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET disabled_at=NULL WHERE id=$1")
        .bind(f.principal.key_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn audit_on() -> bool {
    std::env::var("GATEWAY_SCOPE_LOCK_AUDIT").as_deref() != Ok("off")
}

/// Scoped admission takes no row lock on authority rows (no MultiXacts):
/// while it holds its locks, a concurrent `FOR UPDATE` of the key, workspace
/// and user rows does not wait. (Global mode keeps `FOR SHARE`.)
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scoped_admission_takes_no_authority_row_locks(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    if !scoped(&f.store) {
        return;
    }
    // An admission paused inside its transaction: after its locks, before
    // commit (simulated by the same statements on one connection).
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT * FROM omg_admission_locks($1,$2,$3)")
        .bind(f.principal.workspace_id)
        .bind(f.principal.user_id)
        .bind(f.principal.key_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        crate::auth::revalidate_admission_with(&mut tx, &f.principal, false)
            .await
            .unwrap()
            .is_some()
    );
    let other = tokio::time::timeout(
        Duration::from_millis(500),
        sqlx::query("SELECT 1 FROM api_keys k JOIN workspaces w ON w.id=k.workspace_id JOIN users u ON u.id=$2 WHERE k.id=$1 FOR UPDATE OF k,w,u NOWAIT")
            .bind(f.principal.key_id).bind(f.owner).execute(&pool),
    )
    .await
    .expect("row lock waited");
    assert!(other.is_ok(), "{other:?}");
    tx.rollback().await.unwrap();
}
