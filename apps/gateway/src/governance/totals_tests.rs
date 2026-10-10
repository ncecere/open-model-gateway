//! Maintained budget totals (0015) against the former full scan: random
//! interleavings of real admission/settlement/expiry/reconciliation paths and
//! direct history edits, concurrency, backfill and admission semantics.
use super::totals::{self, Scope, scan_consumption};
use super::*;
use crate::{
    auth::Principal,
    governance::tests::db::{Fixture, done, fixture, request},
    inference::error::LimitScope,
};
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};
use sqlx::{PgPool, migrate::Migrator};

fn start_for(f: &Fixture, principal: Principal) -> ExecutionStart {
    ExecutionStart {
        principal,
        ..f.start()
    }
}
fn failed(id: Uuid) -> ExecutionFinish {
    ExecutionFinish {
        id,
        outcome: Outcome::Failed,
        error: Some(InferenceError::UpstreamUnavailable),
        usage: Usage::default(),
        elapsed_ms: 1,
    }
}

/// Every scope of the fixture: both workspaces and key lineages (there is no
/// installation scope since 0026).
struct Scopes(Vec<(Uuid, Option<Uuid>)>);
impl Scopes {
    async fn of(pool: &PgPool) -> Self {
        let mut scopes = Vec::new();
        let rows: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT DISTINCT workspace_id,governance_key_id FROM api_keys")
                .fetch_all(pool)
                .await
                .unwrap();
        for (ws, lineage) in rows {
            if !scopes.contains(&(ws, None)) {
                scopes.push((ws, None));
            }
            scopes.push((ws, Some(lineage)));
        }
        Self(scopes)
    }
}

/// Totals equal the full scan bucket by bucket, and every scope/period window
/// read by admission equals the former admission-time scan.
async fn assert_consistent(pool: &PgPool, context: &str) {
    let mut tx = pool.begin().await.unwrap();
    let report = totals::verify_in(&mut tx).await.unwrap();
    assert!(report.consistent(), "{context}: {report:#?}");
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    for (ws, lineage) in Scopes::of(pool).await.0 {
        for period in BudgetPeriod::ALL {
            let read = totals::read(&mut tx, &[(Scope::of(ws, lineage), period)], now)
                .await
                .unwrap()[0];
            let (start, end) = period.window(now);
            let (used, unresolved) = scan_consumption(&mut tx, Some(ws), lineage, start, end)
                .await
                .unwrap();
            assert_eq!(
                (read.used_microusd.to_string(), read.unresolved),
                (used, unresolved),
                "{context}: {ws:?}/{lineage:?} {period:?}"
            );
        }
    }
    // Installation-wide spend (alerts, the reservation gauge) is the sum of
    // the workspace rows of a window, exactly the installation-wide scan.
    for period in BudgetPeriod::ALL {
        let (start, end) = period.window(now);
        let summed: (String, bool) = sqlx::query_as("SELECT coalesce(sum(settled_microusd+held_microusd),0)::text,coalesce(sum(unresolved+unreserved_executions),0)>0 FROM budget_totals WHERE scope_kind='workspace' AND period=$1 AND period_start=$2")
            .bind(period.as_str())
            .bind(period.bucket_start(now))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let scanned = scan_consumption(&mut tx, None, None, start, end)
            .await
            .unwrap();
        assert_eq!(summed, scanned, "{context}: installation {period:?}");
    }
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn random_interleavings_keep_totals_equal_to_the_scan(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await; // 110 µUSD hold per admission (100 in + 10 out)
    // A rotated credential in the personal key's lineage.
    let rotated = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'rotated',decode(repeat('05',32),'hex'),$4)")
        .bind(rotated).bind(f.principal.workspace_id).bind(f.owner).bind(f.principal.key_id)
        .execute(&pool).await.unwrap();
    let principals = [
        f.principal,
        f.team,
        Principal {
            key_id: rotated,
            ..f.principal
        },
    ];
    // Budgets large enough that most admissions pass, small enough to deny sometimes.
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.principal.workspace_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    set_test_budget(&pool, "type", Some(&kind), None, None, "day", Some(20_000)).await;
    set_test_budget(
        &pool,
        "local",
        None,
        Some(f.team.workspace_id),
        None,
        "week",
        Some(4_000),
    )
    .await;
    set_test_budget(
        &pool,
        "key",
        None,
        Some(f.principal.workspace_id),
        Some(f.principal.key_id),
        "lifetime",
        Some(6_000),
    )
    .await;
    let mut rng = StdRng::seed_from_u64(0x0015_b0d9e7);
    let mut pending: Vec<(Uuid, Uuid)> = Vec::new(); // (execution, workspace)
    let mut unknown: Vec<(Uuid, Uuid)> = Vec::new();
    let mut all: Vec<Uuid> = Vec::new();
    let mut denied = 0;
    for step in 0..400 {
        let op = rng.gen_range(0..100);
        let what;
        if op < 35 || pending.is_empty() {
            what = "admit";
            let p = *principals.choose(&mut rng).unwrap();
            let s = start_for(&f, p);
            match admit(&f.store, &s, &request(), 3600).await {
                Ok(()) => {
                    pending.push((s.id, p.workspace_id));
                    all.push(s.id);
                }
                Err(InferenceError::BudgetExceeded(_) | InferenceError::UnresolvedUsage(_)) => {
                    denied += 1
                }
                Err(e) => panic!("step {step}: {e:?}"),
            }
        } else if op < 60 {
            what = "settle";
            let (id, _) = pending.swap_remove(rng.gen_range(0..pending.len()));
            let (i, o) = (rng.gen_range(0..120), rng.gen_range(0..60));
            finish(&f.store, &done(id, Some(i), Some(o))).await.unwrap();
        } else if op < 70 {
            what = "fail (unknown keeps hold)";
            let e = pending.swap_remove(rng.gen_range(0..pending.len()));
            finish(&f.store, &failed(e.0)).await.unwrap();
            unknown.push(e);
        } else if op < 76 {
            what = "partial usage (unknown, floor may raise hold)";
            let e = pending.swap_remove(rng.gen_range(0..pending.len()));
            finish(&f.store, &done(e.0, Some(rng.gen_range(0..200)), None))
                .await
                .unwrap();
            unknown.push(e);
        } else if op < 81 {
            what = "expire";
            let e = pending.swap_remove(rng.gen_range(0..pending.len()));
            sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
                .bind(e.0).execute(&pool).await.unwrap();
            assert_eq!(reconcile_expired(&f.store, 10).await.unwrap(), 1);
            unknown.push(e);
        } else if op < 87 && !unknown.is_empty() {
            what = "reconcile";
            let (id, ws) = unknown.swap_remove(rng.gen_range(0..unknown.len()));
            let usage = Usage {
                input_tokens: Some(300),
                output_tokens: Some(rng.gen_range(0..50)),
                ..Default::default()
            };
            resolve_usage(&f.store, ws, id, usage, "receipt:test", f.owner)
                .await
                .unwrap();
        } else if op < 92 {
            what = "unreserved execution";
            let id = Uuid::new_v4();
            let p = principals.choose(&mut rng).unwrap();
            sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',false,'failed',$1,now()-make_interval(days=>$5))")
                .bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(rng.gen_range(0..40)).execute(&pool).await.unwrap();
        } else if op < 97 && !all.is_empty() {
            what = "move admission time (refused)";
            // Admission time is the partition key of the reservation and its
            // ledger (0030): an admitted request's time can never move.
            let id = *all.choose(&mut rng).unwrap();
            let days: i32 = rng.gen_range(1..45);
            assert!(sqlx::query("WITH e AS (UPDATE inference_executions SET started_at=started_at-make_interval(days=>$2) WHERE id=$1) UPDATE governance_reservations SET admitted_at=admitted_at-make_interval(days=>$2) WHERE execution_id=$1").bind(id).bind(days).execute(&pool).await.is_err());
        } else {
            what = "move or delete an unreserved execution";
            let orphan: Option<Uuid> = sqlx::query_scalar("SELECT id FROM inference_executions e WHERE NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id) ORDER BY id LIMIT 1").fetch_optional(&pool).await.unwrap();
            if let Some(id) = orphan {
                if rng.gen_bool(0.5) {
                    sqlx::query("DELETE FROM inference_executions WHERE id=$1")
                        .bind(id)
                        .execute(&pool)
                        .await
                        .unwrap();
                } else {
                    sqlx::query("UPDATE inference_executions SET started_at=started_at-interval '8 days',workspace_id=$2,api_key_id=$3 WHERE id=$1").bind(id).bind(f.team.workspace_id).bind(f.team.key_id).execute(&pool).await.unwrap();
                }
            }
        }
        if step % 5 == 0 || step >= 390 {
            assert_consistent(&pool, &format!("step {step} after {what}")).await;
        }
    }
    assert!(denied > 0, "the interleaving should exercise denials");
    // Unknown cost retains holds in the totals.
    let held: String = sqlx::query_scalar("SELECT sum(held_microusd)::text FROM budget_totals WHERE scope_kind='workspace' AND period='lifetime'").fetch_one(&pool).await.unwrap();
    let expected: String = sqlx::query_scalar("SELECT coalesce(sum(held_microusd),0)::text FROM governance_reservations WHERE state<>'settled'").fetch_one(&pool).await.unwrap();
    assert_eq!(held, expected);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unknown_cost_keeps_its_hold_and_blocks_like_the_scan(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await; // 110 µUSD hold (100 input ceiling + 10 output)
    f.policy("workspace_local_policies", None, None, None, Some(220))
        .await;
    let a = f.start();
    admit(&f.store, &a, &request(), 30).await.unwrap();
    finish(&f.store, &failed(a.id)).await.unwrap();
    let row: (String, String, i64, i64) = sqlx::query_as("SELECT settled_microusd::text,held_microusd::text,unknown,unresolved FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='month'").bind(f.principal.workspace_id).fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("0".into(), "110".into(), 1, 0));
    admit(&f.store, &f.start(), &request(), 30).await.unwrap(); // 110+110 <= 220
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
    // An unbounded unknown reservation (legacy/unpriced) blocks with unresolved_usage.
    let e = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES($1,$2,$3,$4,'company/smart','openai',false,'failed',$1)")
        .bind(e).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).execute(&pool).await.unwrap();
    f.policy(
        "workspace_local_policies",
        None,
        None,
        None,
        Some(1_000_000),
    )
    .await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
    );
    assert_consistent(&pool, "unresolved").await;
}

/// Concurrent admissions/settlements under a tight budget, plus unlocked
/// direct writers on other scopes, never deadlock and never drift.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_writers_keep_totals_exact(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let budget = 110 * 12; // twelve concurrent holds fit
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
    let store = f.store.clone();
    let mut tasks = Vec::new();
    for i in 0..48 {
        let (store, s) = (store.clone(), start_for(&f, f.team));
        tasks.push(tokio::spawn(async move {
            match admit(&store, &s, &request(), 30).await {
                Ok(()) => {
                    let finish_record = if i % 5 == 0 {
                        failed(s.id)
                    } else {
                        done(s.id, Some(10), Some(5))
                    };
                    finish(&store, &finish_record).await.unwrap();
                    true
                }
                Err(InferenceError::BudgetExceeded(LimitScope::Workspace)) => false,
                Err(e) => panic!("{e:?}"),
            }
        }));
    }
    // Writers that bypass the installation lock (owner SQL) on the personal scope.
    for _ in 0..8 {
        let (pool, ws, key, d) = (
            pool.clone(),
            f.principal.workspace_id,
            f.principal.key_id,
            f.deployment,
        );
        tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                sqlx::raw_sql(&format!("DO $$ DECLARE e uuid:=gen_random_uuid(); BEGIN
                  INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES(e,'{ws}','{key}','{d}','company/smart','openai',false,'succeeded',e,now());
                  INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) VALUES(e,'{ws}','{key}','{d}',now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '1 minute','pending',7);
                  UPDATE governance_reservations SET state='settled',actual_microusd=3,input_tokens=1,output_tokens=1 WHERE execution_id=e; END $$"))
                    .execute(&pool).await.unwrap();
            }
            true
        }));
    }
    let mut admitted = 0;
    for t in tasks {
        admitted += usize::from(t.await.unwrap());
    }
    assert!(admitted > 8, "some admissions succeed");
    assert_consistent(&pool, "concurrent").await;
    // Settled + held never exceeds the budget it was admitted under.
    let used: i64 = sqlx::query_scalar("SELECT (settled_microusd+held_microusd)::bigint FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'").bind(f.team.workspace_id).fetch_one(&pool).await.unwrap();
    assert!(used <= budget, "{used} > {budget}");
    let raw: (i64, String) = sqlx::query_as("SELECT reservations,settled_microusd::text FROM budget_totals WHERE scope_kind='key' AND scope_id=$1 AND period='lifetime'").bind(f.principal.key_id).fetch_one(&pool).await.unwrap();
    assert_eq!(raw, (80, "240".into()));
}

/// The migration backfills exactly from existing history (reservations of
/// every state, unreserved executions, rotated lineages, old periods).
#[sqlx::test(migrations = false)]
async fn migration_backfills_existing_history_exactly(pool: PgPool) {
    let before = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version < 15)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&pool).await.unwrap();
    sqlx::raw_sql(r#"DO $$ DECLARE u uuid:=gen_random_uuid(); ws uuid:=gen_random_uuid(); k uuid:=gen_random_uuid(); k2 uuid:=gen_random_uuid(); m uuid:=gen_random_uuid(); pc uuid:=gen_random_uuid(); d uuid:=gen_random_uuid(); e uuid; i integer; BEGIN
     INSERT INTO users(id,email) VALUES(u,'backfill@test.invalid');
     INSERT INTO workspaces(id,name,kind) VALUES(ws,'Backfill','team');
     INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,ws,u,'a',decode(repeat('01',32),'hex'));
     INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES(k2,ws,u,'b',decode(repeat('02',32),'hex'),k);
     INSERT INTO models(id,public_name) VALUES(m,'backfill');
     INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES(pc,'c','openai','env:X');
     INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES(d,m,pc,'x');
     FOR i IN 1..60 LOOP
      e:=gen_random_uuid();
      INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES(e,ws,CASE WHEN i%2=0 THEN k ELSE k2 END,d,'backfill','openai',false,'succeeded',e,now()-make_interval(days=>i));
      IF i%7<>0 THEN
       INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,unbounded_cost,input_tokens,output_tokens)
        VALUES(e,ws,CASE WHEN i%2=0 THEN k ELSE k2 END,d,now()-make_interval(days=>i),now(),now(),now(),
         CASE i%3 WHEN 0 THEN 'settled' WHEN 1 THEN 'unknown' ELSE 'pending' END,
         CASE WHEN i%5=0 THEN NULL ELSE 100+i END,CASE WHEN i%3=0 THEN 9223372036854775807-i ELSE NULL END,i%4=0,CASE WHEN i%3=0 THEN 1 END,CASE WHEN i%3=0 THEN 1 END);
      END IF;
     END LOOP; END $$"#).execute(&pool).await.unwrap();
    crate::store::MIGRATOR.run(&pool).await.unwrap();
    assert_consistent(&pool, "backfill").await;
    // Sums beyond i64 stay exact integers.
    let settled: String = sqlx::query_scalar("SELECT sum(settled_microusd)::text FROM budget_totals WHERE scope_kind='workspace' AND period='lifetime'").fetch_one(&pool).await.unwrap();
    // 0026 removed the installation scope rows the 0015 backfill created.
    let installation: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM budget_totals WHERE scope_kind NOT IN ('workspace','key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(installation, 0);
    let scanned: String = sqlx::query_scalar(
        "SELECT sum(actual_microusd)::text FROM governance_reservations WHERE state='settled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settled, scanned);
    assert!(settled.len() > 19, "exceeds i64: {settled}");
    // History is not modified by the migration; triggers keep it exact afterwards.
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE state='pending'").execute(&pool).await.unwrap();
    assert_consistent(&pool, "after backfill").await;
}
