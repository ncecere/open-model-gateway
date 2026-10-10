//! History parent checks (0034): every replaced foreign key refuses a
//! missing or out-of-scope parent at insert, parents are never deleted or
//! re-keyed, concurrent admissions lock no parent row (no MultiXacts), the
//! verifier and its incident, and the upgrade of a seeded 0033 database.
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::governance::tests::db::{Fixture, done, fixture, request};
use crate::store::Store;
use chrono::TimeDelta;
use sqlx::{PgPool, migrate::Migrator};

type Written = Result<sqlx::postgres::PgQueryResult, sqlx::Error>;

/// SQLSTATE of a failed statement.
fn code(result: Written) -> String {
    match result {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()).unwrap_or_default(),
        other => format!("{other:?}"),
    }
}

struct Exec {
    id: Uuid,
    workspace: Uuid,
    key: Uuid,
    deployment: Uuid,
    cost_center: Option<Uuid>,
    batch_job: Option<Uuid>,
}

impl Exec {
    fn of(f: &Fixture) -> Self {
        Self {
            id: Uuid::now_v7(),
            workspace: f.principal.workspace_id,
            key: f.principal.key_id,
            deployment: f.deployment,
            cost_center: None,
            batch_job: None,
        }
    }
    async fn insert(&self, pool: &PgPool) -> Written {
        sqlx::query("INSERT INTO inference_executions(started_at,id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,cost_center_id,batch_job_id) VALUES(now(),$1,$2,$3,$4,'company/smart','openai',false,'started',$1,$5,$6)")
            .bind(self.id).bind(self.workspace).bind(self.key).bind(self.deployment).bind(self.cost_center).bind(self.batch_job)
            .execute(pool).await
    }
}

async fn reserve(pool: &PgPool, e: &Exec, price: Option<Uuid>) -> Written {
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,$2,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at+interval '1 minute','pending',110,1 FROM inference_executions WHERE id=$1")
        .bind(e.id).bind(price).execute(pool).await
}

/// A second deployment (of the fixture's model) with its own price version.
async fn other_deployment(pool: &PgPool, f: &Fixture) -> (Uuid, Uuid) {
    let (d, p) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,model_id,provider_connection_id,'other-model',true FROM deployments WHERE id=$2")
        .bind(d).bind(f.deployment).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1,1,100,50,1)")
        .bind(p).bind(d).execute(pool).await.unwrap();
    (d, p)
}

/// A batch job of the personal (or team) workspace, with the envelope
/// execution it requires.
async fn batch_job(pool: &PgPool, f: &Fixture, team: bool) -> Uuid {
    let p = if team { f.team } else { f.principal };
    let mut envelope = Exec::of(f);
    envelope.workspace = p.workspace_id;
    envelope.key = p.key_id;
    envelope.insert(pool).await.unwrap();
    let job = Uuid::new_v4();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint) VALUES($1,'batch',$2,$3,$4,$5,'company/smart','openai','batch_up_'||replace($1::text,'-',''),now()+interval '26 hours','/v1/chat/completions')")
        .bind(job).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(envelope.id).execute(pool).await.unwrap();
    job
}

async fn cost_center(pool: &PgPool, code: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,$2,$2)")
        .bind(id)
        .bind(code)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// Every replaced foreign key: a missing parent and an out-of-scope parent
/// are refused with 23503 at insert (and at an update of the reference);
/// valid references are accepted.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn history_inserts_check_scoped_parents(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let price = f.price(1_000_000).await;
    let (other_d, other_price) = other_deployment(&pool, &f).await;
    let center = cost_center(&pool, "R-1").await;
    let own_job = batch_job(&pool, &f, false).await;
    let team_job = batch_job(&pool, &f, true).await;

    // Valid: every reference present and in scope.
    let mut ok = Exec::of(&f);
    ok.cost_center = Some(center);
    ok.batch_job = Some(own_job);
    ok.insert(&pool).await.unwrap();
    reserve(&pool, &ok, Some(price)).await.unwrap();
    let unpriced = Exec::of(&f);
    unpriced.insert(&pool).await.unwrap();
    reserve(&pool, &unpriced, None).await.unwrap();

    // executions -> workspaces / api_keys(workspace_id,id)
    let mut e = Exec::of(&f);
    e.key = Uuid::new_v4();
    assert_eq!(code(e.insert(&pool).await), "23503", "missing key");
    let mut e = Exec::of(&f);
    e.key = f.team.key_id;
    assert_eq!(
        code(e.insert(&pool).await),
        "23503",
        "key of another workspace"
    );
    let mut e = Exec::of(&f);
    e.workspace = Uuid::new_v4();
    assert_eq!(code(e.insert(&pool).await), "23503", "missing workspace");
    // executions -> deployments
    let mut e = Exec::of(&f);
    e.deployment = Uuid::new_v4();
    assert_eq!(code(e.insert(&pool).await), "23503", "missing deployment");
    // executions -> cost_centers
    let mut e = Exec::of(&f);
    e.cost_center = Some(Uuid::new_v4());
    assert_eq!(code(e.insert(&pool).await), "23503", "missing cost center");
    // executions -> async_jobs(workspace_id,id)
    let mut e = Exec::of(&f);
    e.batch_job = Some(Uuid::new_v4());
    assert_eq!(code(e.insert(&pool).await), "23503", "missing batch job");
    let mut e = Exec::of(&f);
    e.batch_job = Some(team_job);
    assert_eq!(
        code(e.insert(&pool).await),
        "23503",
        "batch job of another workspace"
    );
    // reservations -> deployment_prices(deployment_id,id)
    let e = Exec::of(&f);
    e.insert(&pool).await.unwrap();
    assert_eq!(
        code(reserve(&pool, &e, Some(Uuid::new_v4())).await),
        "23503",
        "missing price"
    );
    assert_eq!(
        code(reserve(&pool, &e, Some(other_price)).await),
        "23503",
        "price of another deployment"
    );
    let mut moved = Exec::of(&f);
    moved.deployment = other_d;
    moved.insert(&pool).await.unwrap();
    reserve(&pool, &moved, Some(other_price)).await.unwrap();

    // Updates of the references are checked too (the runtime cannot write
    // them at all; this covers owner sessions).
    for (sql, what) in [
        (
            "UPDATE inference_executions SET api_key_id=$2 WHERE id=$1",
            "key",
        ),
        (
            "UPDATE inference_executions SET deployment_id=$2 WHERE id=$1",
            "deployment",
        ),
        (
            "UPDATE inference_executions SET cost_center_id=$2 WHERE id=$1",
            "cost center",
        ),
        (
            "UPDATE inference_executions SET batch_job_id=$2 WHERE id=$1",
            "batch job",
        ),
    ] {
        let r = sqlx::query(sql)
            .bind(unpriced.id)
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await;
        assert_eq!(code(r), "23503", "update of the {what}");
    }
    let r = sqlx::query("UPDATE governance_reservations SET price_id=$2 WHERE execution_id=$1")
        .bind(unpriced.id)
        .bind(other_price)
        .execute(&pool)
        .await;
    assert_eq!(code(r), "23503", "update of the price");

    // The hot foreign keys are gone; the history-to-history keys remain.
    let keys: Vec<String> = sqlx::query_scalar("SELECT conname::text FROM pg_constraint WHERE contype='f' AND conparentid=0 AND conrelid IN ('inference_executions'::regclass,'governance_reservations'::regclass,'monetary_ledger'::regclass) ORDER BY 1")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        keys,
        [
            "governance_reservations_execution_fkey",
            "monetary_ledger_execution_fkey"
        ]
    );
    let report = verify(&pool, None).await.unwrap();
    assert!(report.consistent(), "{report:#?}");
}

/// Parents are never deleted, truncated or re-keyed, by any role and also
/// when unreferenced, so a plain existence check cannot race a removal.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn history_parents_are_never_removed_or_rekeyed(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let price = f.price(1_000_000).await;
    let center = cost_center(&pool, "O-1").await;
    // An unreferenced workspace (no keys, no history).
    let spare = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Spare','project')")
        .bind(spare)
        .execute(&pool)
        .await
        .unwrap();
    let job = batch_job(&pool, &f, false).await;
    for (sql, id, what) in [
        ("DELETE FROM workspaces WHERE id=$1", spare, "workspace"),
        ("DELETE FROM api_keys WHERE id=$1", f.team.key_id, "key"),
        (
            "DELETE FROM deployments WHERE id=$1",
            f.deployment,
            "deployment",
        ),
        (
            "DELETE FROM cost_centers WHERE id=$1",
            center,
            "cost center",
        ),
        (
            "UPDATE workspaces SET id=gen_random_uuid() WHERE id=$1",
            spare,
            "workspace id",
        ),
        (
            "UPDATE api_keys SET id=gen_random_uuid() WHERE id=$1",
            f.team.key_id,
            "key id",
        ),
        (
            "UPDATE deployments SET id=gen_random_uuid() WHERE id=$1",
            f.deployment,
            "deployment id",
        ),
        (
            "UPDATE cost_centers SET id=gen_random_uuid() WHERE id=$1",
            center,
            "cost center id",
        ),
    ] {
        let r = sqlx::query(sql).bind(id).execute(&pool).await;
        assert_eq!(code(r), "23503", "{what}");
    }
    // A key never moves to another workspace (the scope of its history).
    let r = sqlx::query("UPDATE api_keys SET workspace_id=$2 WHERE id=$1")
        .bind(f.team.key_id)
        .bind(spare)
        .execute(&pool)
        .await;
    assert_eq!(code(r), "23503", "key workspace");
    // Already immutable before 0034: price versions and jobs.
    for (sql, id) in [
        ("DELETE FROM deployment_prices WHERE id=$1", price),
        (
            "UPDATE deployment_prices SET id=gen_random_uuid() WHERE id=$1",
            price,
        ),
        ("DELETE FROM async_jobs WHERE id=$1", job),
        (
            "UPDATE async_jobs SET id=gen_random_uuid() WHERE id=$1",
            job,
        ),
    ] {
        assert!(
            sqlx::query(sql).bind(id).execute(&pool).await.is_err(),
            "{sql}"
        );
    }
    for table in [
        "workspaces",
        "api_keys",
        "deployments",
        "cost_centers",
        "async_jobs",
    ] {
        let mut tx = pool.begin().await.unwrap();
        let r = sqlx::query(&format!("TRUNCATE {table} CASCADE"))
            .execute(&mut *tx)
            .await;
        assert!(r.is_err(), "TRUNCATE {table}");
    }
    // Ordinary lifecycle updates are unaffected.
    for (sql, id) in [
        ("UPDATE workspaces SET disabled_at=now() WHERE id=$1", spare),
        (
            "UPDATE api_keys SET revoked_at=now() WHERE id=$1",
            f.team.key_id,
        ),
        (
            "UPDATE deployments SET enabled=false WHERE id=$1",
            f.deployment,
        ),
        (
            "UPDATE cost_centers SET archived_at=now() WHERE id=$1",
            center,
        ),
    ] {
        sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
    }
    let left: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM workspaces WHERE id=$1)+(SELECT count(*) FROM api_keys WHERE id=$2)+(SELECT count(*) FROM deployments WHERE id=$3)+(SELECT count(*) FROM cost_centers WHERE id=$4)")
        .bind(spare).bind(f.team.key_id).bind(f.deployment).bind(center).fetch_one(&pool).await.unwrap();
    assert_eq!(left, 4);
}

/// (block, line pointer, t_xmax, xmax is a MultiXact) of every tuple of
/// `table` (pageinspect). A new row lock (`FOR KEY SHARE` from a foreign-key
/// check, or a MultiXact of several lockers) changes a tuple's `t_xmax`;
/// plain reads never do.
async fn xmax(pool: &PgPool, table: &str) -> Vec<(i64, i32, i64, bool)> {
    sqlx::query_as("SELECT b,i.lp::int,coalesce(i.t_xmax::text::bigint,0),coalesce(i.t_infomask&4096<>0,false) FROM generate_series(0,(pg_relation_size($1::regclass)/current_setting('block_size')::int)-1) b CROSS JOIN LATERAL heap_page_items(get_raw_page($1,b::int)) i ORDER BY 1,2")
        .bind(table).fetch_all(pool).await.unwrap()
}

/// Leaf partitions of a partitioned history table.
async fn leaves(pool: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT relid::regclass::text FROM pg_partition_tree($1::regclass) WHERE isleaf",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// D3: a burst of concurrent admissions and settlements through the real
/// code path takes no row lock on any parent (workspaces, keys, deployments,
/// price versions, cost centers: every tuple's `t_xmax` is unchanged) and
/// creates no MultiXact on history rows. A positive control shows the probe
/// detects the `FOR KEY SHARE` MultiXacts a foreign key creates. (Global
/// admission mode reads parents `FOR SHARE` by design and is not probed.)
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_admissions_lock_no_parent_rows(pool: PgPool) {
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pageinspect")
        .execute(&pool)
        .await
        .unwrap();
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let center = cost_center(&pool, "H-1").await;
    sqlx::query("UPDATE workspaces SET cost_center_id=$1")
        .bind(center)
        .execute(&pool)
        .await
        .unwrap();

    // Positive control: overlapping inserts through a foreign key to the
    // hot deployment row leave a MultiXact in its t_xmax.
    sqlx::query("CREATE TABLE d3_probe(id uuid PRIMARY KEY, deployment_id uuid NOT NULL REFERENCES deployments(id))")
        .execute(&pool).await.unwrap();
    let before = xmax(&pool, "deployments").await;
    let mut open = Vec::new();
    for _ in 0..3 {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO d3_probe VALUES(gen_random_uuid(),$1)")
            .bind(f.deployment)
            .execute(&mut *tx)
            .await
            .unwrap();
        open.push(tx);
    }
    let locked = xmax(&pool, "deployments").await;
    for tx in open {
        tx.commit().await.unwrap();
    }
    assert_ne!(before, locked, "control: the foreign key locked the parent");
    assert!(
        locked.iter().any(|t| t.3),
        "control: overlapping lockers made a MultiXact"
    );
    sqlx::query("DROP TABLE d3_probe")
        .execute(&pool)
        .await
        .unwrap();
    if f.store.admission_mode() != crate::governance::locks::AdmissionMode::Scoped {
        return;
    }

    const PARENTS: [&str; 5] = [
        "workspaces",
        "api_keys",
        "deployments",
        "deployment_prices",
        "cost_centers",
    ];
    let mut before = Vec::new();
    for t in PARENTS {
        before.push(xmax(&pool, t).await);
    }
    let wide = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let store = Store::new(wide);
    let mut tasks = Vec::new();
    for i in 0..64 {
        let principal = if i % 2 == 0 { f.principal } else { f.team };
        let store = store.clone();
        let start = crate::inference::repository::ExecutionStart {
            principal,
            ..f.start()
        };
        tasks.push(tokio::spawn(async move {
            crate::governance::admit(&store, &start, &request(), 60)
                .await
                .unwrap();
            crate::governance::finish(&store, &done(start.id, Some(10), Some(5)))
                .await
                .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let settled: i64 =
        sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE state='settled'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settled, 64);
    for (t, b) in PARENTS.iter().zip(before) {
        assert_eq!(xmax(&pool, t).await, b, "{t}: a parent row was locked");
    }
    for table in [
        "inference_executions",
        "governance_reservations",
        "monetary_ledger",
    ] {
        for leaf in leaves(&pool, table).await {
            assert!(
                xmax(&pool, &leaf).await.iter().all(|t| !t.3),
                "{leaf}: MultiXact on a history row"
            );
        }
    }
    let report = verify(&pool, None).await.unwrap();
    assert!(report.consistent(), "{report:#?}");
}

/// The verifier finds orphans and scope mismatches written around the
/// triggers (replication role) and a disabled guard; the job opens one
/// incident with one delivery; only a full clean run resolves it.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn verify_finds_orphans_and_raises_the_incident(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let (_, other_price) = other_deployment(&pool, &f).await;
    let a = f.start();
    crate::governance::admit(&f.store, &a, &request(), 60)
        .await
        .unwrap();
    crate::governance::finish(&f.store, &done(a.id, Some(10), Some(5)))
        .await
        .unwrap();
    let clean = verify(&pool, None).await.unwrap();
    assert!(clean.consistent(), "{clean:#?}");
    assert_eq!(
        (clean.executions_checked, clean.reservations_checked),
        (1, 1)
    );

    // Bypass the checks (an owner session with triggers off).
    let mut bad_key = Exec::of(&f);
    bad_key.key = f.team.key_id;
    let mut bad_deployment = Exec::of(&f);
    bad_deployment.deployment = Uuid::new_v4();
    let mut bad_job = Exec::of(&f);
    bad_job.batch_job = Some(Uuid::new_v4());
    let bad_price = Exec::of(&f);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    for e in [&bad_key, &bad_deployment, &bad_job, &bad_price] {
        sqlx::query("INSERT INTO inference_executions(started_at,id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,batch_job_id) VALUES(now(),$1,$2,$3,$4,'m','openai',false,'started',$1,$5)")
            .bind(e.id).bind(e.workspace).bind(e.key).bind(e.deployment).bind(e.batch_job).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state) SELECT id,workspace_id,api_key_id,deployment_id,$2,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at+interval '1 minute','pending' FROM inference_executions WHERE id=$1")
        .bind(bad_price.id).bind(other_price).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("ALTER TABLE deployments DISABLE TRIGGER deployments_history_parent")
        .execute(&pool)
        .await
        .unwrap();

    let report = verify(&pool, None).await.unwrap();
    let found: Vec<(&str, i64, Vec<Uuid>)> = report
        .findings
        .iter()
        .map(|x| (x.check.as_str(), x.count, x.sample_ids.clone()))
        .collect();
    assert_eq!(
        found,
        vec![
            ("execution_key_not_in_workspace", 1, vec![bad_key.id]),
            ("execution_deployment_missing", 1, vec![bad_deployment.id]),
            ("execution_batch_job_not_in_workspace", 1, vec![bad_job.id]),
            ("reservation_price_not_of_deployment", 1, vec![bad_price.id]),
            ("enforcement_missing:deployments_history_parent", 1, vec![]),
        ],
        "{report:#?}"
    );
    assert_eq!(report.finding_count, 5);
    // A window after the orphans finds only the enforcement gap.
    let later = verify(&pool, Some(Utc::now() + TimeDelta::minutes(1)))
        .await
        .unwrap();
    assert_eq!(later.finding_count, 1, "{later:#?}");

    // The leased job (no fence: a single replica) opens one incident with
    // one delivery, also when it runs again.
    let job = run_job(&f.store, None).await.unwrap();
    assert_eq!(job.finding_count, 5);
    run_job(&f.store, None).await.unwrap();
    let open: Vec<(String, i64)> = sqlx::query_as("SELECT e.severity,(SELECT count(*) FROM alert_deliveries d WHERE d.event_id=e.id) FROM alert_events e WHERE e.builtin='history_orphans' AND e.resolved_at IS NULL")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(open, vec![("critical".to_owned(), 1)]);

    // Repair; a windowed clean run keeps the incident, a full one resolves it.
    sqlx::query("ALTER TABLE deployments ENABLE TRIGGER deployments_history_parent")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    let ids = [bad_key.id, bad_deployment.id, bad_job.id, bad_price.id];
    sqlx::query("DELETE FROM governance_reservations WHERE execution_id=ANY($1)")
        .bind(&ids[..])
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM inference_executions WHERE id=ANY($1)")
        .bind(&ids[..])
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let open_count = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_events WHERE builtin='history_orphans' AND resolved_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let windowed = verify(&pool, Some(Utc::now())).await.unwrap();
    record_cli_result(&f.store, &windowed).await.unwrap();
    assert_eq!(open_count().await, 1);
    let full = verify(&pool, None).await.unwrap();
    assert!(full.consistent(), "{full:#?}");
    record_cli_result(&f.store, &full).await.unwrap();
    assert_eq!(open_count().await, 0);
}

/// The 0034 upgrade of a seeded 0033 database (`OMG_P6B_MIGRATION_ROWS`
/// attempts, default 3000): history and totals unchanged, the hot keys
/// replaced, the verifier clean, admission works; the duration and the
/// table locks seen are reported as JSON on stderr.
#[sqlx::test(migrations = false)]
async fn seeded_history_upgrades_to_parent_checks(pool: PgPool) {
    let rows: i64 = std::env::var("OMG_P6B_MIGRATION_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let before = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version <= 33)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&pool).await.unwrap();
    let f = fixture(pool.clone()).await;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut seeded = 0;
    while seeded < rows {
        let n = 200_000i64.min(rows - seeded);
        crate::partitions::tests::seed_history(
            &pool,
            &f,
            &format!("u{seeded}-"),
            n,
            now - TimeDelta::days(60),
            60 * 86_400 - 600,
            true,
        )
        .await;
        seeded += n;
    }
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();
    const STATS: &str = "SELECT concat_ws(',',(SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM monetary_ledger),(SELECT coalesce(sum(amount_microusd),0) FROM monetary_ledger),(SELECT coalesce(sum(actual_microusd),0) FROM governance_reservations),(SELECT sum(settled_microusd+held_microusd) FROM budget_totals))";
    let stats_before: String = sqlx::query_scalar(STATS).fetch_one(&pool).await.unwrap();
    // Table locks other sessions would wait for, sampled while upgrading.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (pool, stop) = (pool.clone(), stop.clone());
        tokio::spawn(async move {
            let mut seen = std::collections::BTreeSet::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let held: Vec<String> = sqlx::query_scalar("SELECT DISTINCT c.relname||':'||l.mode FROM pg_locks l JOIN pg_class c ON c.oid=l.relation WHERE l.granted AND l.pid<>pg_backend_pid() AND l.database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND l.mode IN ('AccessExclusiveLock','ExclusiveLock','ShareLock','ShareRowExclusiveLock') AND c.relkind IN ('r','p')")
                    .fetch_all(&pool).await.unwrap_or_default();
                seen.extend(held);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            seen
        })
    };
    let store = Store::new(pool.clone());
    let started = std::time::Instant::now();
    store.migrate_enterprise().await.unwrap();
    let migrate_ms = started.elapsed().as_millis();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let locks = sampler.await.unwrap();
    assert!(store.is_ready().await);
    let stats_after: String = sqlx::query_scalar(STATS).fetch_one(&pool).await.unwrap();
    assert_eq!(stats_before, stats_after);
    let hot: Vec<String> = sqlx::query_scalar("SELECT conrelid::regclass||'.'||conname FROM pg_constraint WHERE contype='f' AND conparentid=0 AND conrelid IN ('inference_executions'::regclass,'governance_reservations'::regclass) AND confrelid NOT IN ('inference_executions'::regclass,'governance_reservations'::regclass)")
        .fetch_all(&pool).await.unwrap();
    assert!(hot.is_empty(), "hot parent keys remain: {hot:?}");
    let report = verify(&pool, None).await.unwrap();
    assert!(report.consistent(), "{report:#?}");
    assert_eq!(report.executions_checked, rows);
    let mut tx = pool.begin().await.unwrap();
    let totals = crate::governance::totals::verify_in(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(totals.consistent(), "{totals:#?}");
    let a = f.start();
    crate::governance::admit(&f.store, &a, &request(), 60)
        .await
        .unwrap();
    crate::governance::finish(&f.store, &done(a.id, Some(10), Some(5)))
        .await
        .unwrap();
    let step_ms: i64 =
        sqlx::query_scalar("SELECT execution_time/1000000 FROM _sqlx_migrations WHERE version=34")
            .fetch_one(&pool)
            .await
            .unwrap();
    eprintln!(
        "P6b migration measurement: {}",
        serde_json::json!({
            "attempts": rows, "migrate_ms": migrate_ms, "migration_0034_ms": step_ms,
            "locks_seen": locks, "history_verify_seconds": report.seconds,
            "executions_checked": report.executions_checked,
        })
    );
    f.store.pool.close().await;
}
