//! Maintained rate and in-flight counters (0024) against the former
//! `RATE_ACCOUNTING` scan: random interleavings of real admission/settlement/
//! expiry/reconciliation paths, async-job and batch-line lifecycles and direct
//! history edits; minute and lease boundaries on the pinned admission clock;
//! backfill; batched expiry reconciliation.
use super::rates::{self, Counters, Limits, SCAN, scan_counters};
use super::totals;
use super::*;
use crate::{
    auth::Principal,
    governance::tests::db::{Fixture, done, fixture, request},
};
use rand::{RngExt, SeedableRng, rngs::StdRng, seq::IndexedRandom};
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

/// Every scope: each workspace and each key lineage (no installation scope).
async fn scopes(pool: &PgPool) -> Vec<(Option<Uuid>, Option<Uuid>)> {
    let mut scopes = Vec::new();
    let rows: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT DISTINCT workspace_id,governance_key_id FROM api_keys ORDER BY 1,2")
            .fetch_all(pool)
            .await
            .unwrap();
    for (ws, lineage) in rows {
        if !scopes.contains(&(Some(ws), None)) {
            scopes.push((Some(ws), None));
        }
        scopes.push((Some(ws), Some(lineage)));
    }
    scopes
}

/// The scan's verdict for one layer.
async fn scan_verdict(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    (ws, lineage): (Option<Uuid>, Option<Uuid>),
    at: DateTime<Utc>,
    l: Limits,
    reserved: Option<i64>,
) -> (bool, bool) {
    sqlx::query_as(SCAN)
        .bind(ws)
        .bind(lineage)
        .bind(at)
        .bind(l.requests_per_minute)
        .bind(l.tokens_per_minute)
        .bind(l.concurrent_requests)
        .bind(reserved)
        .bind(l.concurrent_jobs)
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

/// Counters equal the scan's quantities for every scope at `at`, and the
/// verdicts agree at and around every limit.
async fn assert_parity(pool: &PgPool, at: DateTime<Utc>, context: &str) {
    let mut tx = pool.begin().await.unwrap();
    let all = scopes(pool).await;
    let mut read = Vec::new();
    for scope in &all {
        read.push(match *scope {
            (Some(ws), None) => Some(
                rates::read_counters(&mut tx, ws, Uuid::nil(), at)
                    .await
                    .unwrap()
                    .0,
            ),
            (Some(ws), Some(l)) => Some(rates::read_counters(&mut tx, ws, l, at).await.unwrap().1),
            _ => None,
        });
    }
    for (scope, maintained) in all.iter().zip(&read) {
        let Some(maintained) = maintained else {
            continue;
        };
        let scanned: Counters = scan_counters(&mut tx, scope.0, scope.1, at).await.unwrap();
        assert_eq!(*maintained, scanned, "{context}: {scope:?} at {at}");
        // Each limit just below, at and above its threshold (alone and all
        // together): the verdicts are the scan's.
        let reserved = 110i64;
        let at_limit = |n: i128, k: i64| i64::try_from(n).unwrap() + k;
        let mut cases = Vec::new();
        for k in [-1, 0, 1] {
            let rpm = Some(at_limit(i128::from(scanned.requests), k).max(0));
            let tpm = Some(at_limit(scanned.tokens + i128::from(reserved) - 1, k).max(0));
            let concurrent = Some(at_limit(i128::from(scanned.inflight), k).max(0));
            let jobs = Some(at_limit(i128::from(scanned.jobs), k).max(0));
            let none = Limits::default();
            cases.push(Limits {
                requests_per_minute: rpm,
                ..none
            });
            cases.push(Limits {
                tokens_per_minute: tpm,
                ..none
            });
            cases.push(Limits {
                concurrent_requests: concurrent,
                ..none
            });
            cases.push(Limits {
                concurrent_jobs: jobs,
                ..none
            });
            cases.push(Limits {
                requests_per_minute: rpm,
                tokens_per_minute: tpm,
                concurrent_requests: concurrent,
                concurrent_jobs: jobs,
            });
        }
        for l in cases {
            assert_eq!(
                maintained.admits(l, Some(reserved)),
                scan_verdict(&mut tx, *scope, at, l, Some(reserved)).await,
                "{context}: {scope:?} at {at} {l:?}"
            );
        }
    }
    let report = totals::verify_in(&mut tx).await.unwrap();
    assert!(report.consistent(), "{context}: {report:#?}");
    tx.rollback().await.unwrap();
}

async fn insert_execution(
    pool: &PgPool,
    f: &Fixture,
    p: Principal,
    kind: &str,
    started_at: DateTime<Utc>,
    batch_job: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,workload_kind,batch_job_id) VALUES($1,$2,$3,$4,'company/smart','openai',false,'started',$1,$5,$6,$7)")
        .bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(started_at).bind(kind).bind(batch_job)
        .execute(pool).await.unwrap();
    id
}

async fn insert_reservation(
    pool: &PgPool,
    f: &Fixture,
    p: Principal,
    id: Uuid,
    minute: DateTime<Utc>,
    lease: DateTime<Utc>,
    reserved: Option<i64>,
) {
    // Admitted at the execution's start (0030 key); the minute is the test's.
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,unbounded_cost) SELECT $1,$2,$3,$4,e.started_at,$5,date_trunc('month',$5::timestamptz,'UTC'),$6,'pending',$7,7,false FROM inference_executions e WHERE e.id=$1")
        .bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(minute).bind(lease).bind(reserved)
        .execute(pool).await.unwrap();
}

async fn insert_job(
    pool: &PgPool,
    f: &Fixture,
    p: Principal,
    execution: Uuid,
    video: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    if video {
        sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,video_seconds,video_size) VALUES($1,'video',$2,$3,$4,$5,'company/smart','openai',$6,now()+interval '1 day',4,'1280x720')")
            .bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(execution).bind(format!("up-{id}"))
            .execute(pool).await.unwrap();
    } else {
        sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint) VALUES($1,'batch',$2,$3,$4,$5,'company/smart','openai',$6,now()+interval '1 day','/v1/chat/completions')")
            .bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(execution).bind(format!("up-{id}"))
            .execute(pool).await.unwrap();
    }
    id
}

const BILLING: &str = r#"{"total_input_tokens":"500","uncached_input_tokens":"100","cache_read_input_tokens":"300","cache_write_input_tokens":"100","cache_write_default_input_tokens":null,"cache_write_5m_input_tokens":null,"cache_write_1h_input_tokens":null}"#;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn random_interleavings_keep_rate_counters_equal_to_the_scan(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await; // 110 reserved tokens (100 input ceiling + 10 output)
    let now = f.store.freeze_admission_clock().await.unwrap();
    let minute: DateTime<Utc> =
        sqlx::query_scalar("SELECT date_trunc('minute',$1::timestamptz,'UTC')")
            .bind(now)
            .fetch_one(&pool)
            .await
            .unwrap();
    let minute_s = chrono::TimeDelta::minutes(1);
    // Limits high enough to admit, so admission reads the counters. The
    // personal workspace's tokens/minute denies while its minute holds an
    // unreserved execution or unpriced reservation (as the scan did).
    f.policy(
        "workspace_platform_policy_overrides",
        Some(1_000_000),
        None,
        Some(1_000_000),
        None,
    )
    .await;
    f.policy(
        "workspace_local_policies",
        None,
        Some(1_000_000_000),
        None,
        None,
    )
    .await;
    let mut busy = 0;
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
    let mut rng = StdRng::seed_from_u64(0x0024_7a7e);
    let mut pending: Vec<Uuid> = Vec::new();
    let mut jobs: Vec<Uuid> = Vec::new(); // async job ids
    let mut batches: Vec<(Uuid, Principal)> = Vec::new();
    let mut all: Vec<Uuid> = Vec::new();
    let instants = [
        now,
        now - minute_s,
        now + minute_s,
        now + chrono::TimeDelta::seconds(30),
        now + chrono::TimeDelta::seconds(3600),
    ];
    for step in 0..300 {
        let op = rng.random_range(0..100);
        let p = *principals.choose(&mut rng).unwrap();
        let what;
        if op < 25 || pending.is_empty() {
            what = "admit";
            let s = start_for(&f, p);
            match admit(&f.store, &s, &request(), rng.random_range(1..90)).await {
                Ok(()) => {
                    pending.push(s.id);
                    all.push(s.id);
                }
                Err(InferenceError::Busy) if p.workspace_id == f.principal.workspace_id => {
                    busy += 1
                }
                Err(e) => panic!("step {step}: {e:?}"),
            }
        } else if op < 37 && !pending.is_empty() {
            what = "settle (observed excess raises tokens)";
            let id = pending.swap_remove(rng.random_range(0..pending.len()));
            let _ = finish(
                &f.store,
                &done(
                    id,
                    Some(rng.random_range(0..100)),
                    Some(rng.random_range(0..50)),
                ),
            )
            .await;
        } else if op < 42 && !pending.is_empty() {
            what = "fail (unknown)";
            let id = pending.swap_remove(rng.random_range(0..pending.len()));
            let _ = finish(&f.store, &failed(id)).await;
        } else if op < 47 && !pending.is_empty() {
            what = "expire lease (with or without reconciliation)";
            let id = *pending.choose(&mut rng).unwrap();
            let offset = [-61, -30, -1, 0, 1, 30][rng.random_range(0..6)];
            sqlx::query("UPDATE governance_reservations SET lease_expires_at=$2 WHERE execution_id=$1 AND state='pending'")
                .bind(id).bind(now + chrono::TimeDelta::seconds(offset)).execute(&pool).await.unwrap();
            if rng.random_bool(0.3) {
                reconcile_expired(&f.store, 100).await.unwrap();
            }
        } else if op < 55 {
            what = "async job admission (video or native batch)";
            let video = rng.random_bool(0.5);
            let at = instants[rng.random_range(0..3)];
            let e = insert_execution(
                &pool,
                &f,
                p,
                if video { "videos" } else { "batches" },
                at,
                None,
            )
            .await;
            insert_reservation(
                &pool,
                &f,
                p,
                e,
                minute + minute_s * rng.random_range(-1..2),
                now + chrono::TimeDelta::seconds(rng.random_range(-5..4000)),
                Some(rng.random_range(0..500)),
            )
            .await;
            all.push(e);
            if rng.random_bool(0.7) {
                let j = insert_job(&pool, &f, p, e, video).await;
                jobs.push(j);
                if !video && rng.random_bool(0.5) {
                    batches.push((j, p));
                }
            }
        } else if op < 60 && !jobs.is_empty() {
            what = "job progresses, finishes or is cancel-requested";
            let j = *jobs.choose(&mut rng).unwrap();
            match rng.random_range(0..3) {
                0 => sqlx::query("UPDATE async_jobs SET state='in_progress' WHERE id=$1 AND state='queued'"),
                1 => sqlx::query("UPDATE async_jobs SET state='completed',completed_at=now() WHERE id=$1 AND state IN('queued','in_progress')"),
                _ => sqlx::query("UPDATE async_jobs SET cancel_requested_at=coalesce(cancel_requested_at,now()) WHERE id=$1"),
            }
            .bind(j)
            .execute(&pool)
            .await
            .unwrap();
        } else if op < 65 && !batches.is_empty() {
            what = "gateway-run batch line";
            let (j, owner) = *batches.choose(&mut rng).unwrap();
            let e = insert_execution(&pool, &f, owner, "generation", now, Some(j)).await;
            insert_reservation(
                &pool,
                &f,
                owner,
                e,
                minute,
                now + chrono::TimeDelta::seconds(60),
                Some(150),
            )
            .await;
            all.push(e);
        } else if op < 72 {
            what = "unreserved execution";
            let at = instants[rng.random_range(0..3)]
                + chrono::TimeDelta::seconds(rng.random_range(-20..20));
            insert_execution(&pool, &f, p, "generation", at, None).await;
        } else if op < 77 {
            what = "unpriced or untruncated-minute reservation";
            let e = insert_execution(&pool, &f, p, "generation", now, None).await;
            let at = if rng.random_bool(0.5) { minute } else { now };
            let reserved = if rng.random_bool(0.5) { None } else { Some(40) };
            insert_reservation(
                &pool,
                &f,
                p,
                e,
                at,
                now + chrono::TimeDelta::seconds(120),
                reserved,
            )
            .await;
            all.push(e);
        } else if op < 82 && !pending.is_empty() {
            what = "window growth or normalized usage on a pending reservation";
            let id = *pending.choose(&mut rng).unwrap();
            if rng.random_bool(0.5) {
                sqlx::query("UPDATE governance_reservations SET reserved_tokens=reserved_tokens+$2 WHERE execution_id=$1 AND state='pending'")
                    .bind(id).bind(rng.random_range(0..300i64)).execute(&pool).await.unwrap();
            } else {
                sqlx::query("UPDATE governance_reservations SET input_tokens=50,output_tokens=$2,billing_usage=$3::jsonb WHERE execution_id=$1 AND state='pending'")
                    .bind(id).bind(rng.random_range(0..400i64)).bind(BILLING).execute(&pool).await.unwrap();
            }
        } else if op < 88 && !all.is_empty() {
            what = "move reservation minute (execution start refused)";
            let id = *all.choose(&mut rng).unwrap();
            let delta = minute_s * rng.random_range(-1..2);
            sqlx::query("UPDATE governance_reservations SET minute_start=minute_start+$2 WHERE execution_id=$1")
                .bind(id).bind(delta).execute(&pool).await.unwrap();
            // A reserved execution's start is its reservation's admission time (0030 key).
            if delta != chrono::TimeDelta::zero() {
                assert!(
                    sqlx::query(
                        "UPDATE inference_executions SET started_at=started_at+$2 WHERE id=$1"
                    )
                    .bind(id)
                    .bind(delta)
                    .execute(&pool)
                    .await
                    .is_err()
                );
            }
        } else if op < 93 && !all.is_empty() {
            what = "change workload or batch link";
            let id = *all.choose(&mut rng).unwrap();
            let kind = ["generation", "videos", "batches", "embeddings"][rng.random_range(0..4)];
            sqlx::query("UPDATE inference_executions SET workload_kind=$2 WHERE id=$1")
                .bind(id)
                .bind(kind)
                .execute(&pool)
                .await
                .unwrap();
            if let Some((j, owner)) = batches.choose(&mut rng).copied() {
                sqlx::query("UPDATE inference_executions SET batch_job_id=CASE WHEN batch_job_id IS NULL THEN $2 END WHERE id=$1 AND workspace_id=$3")
                    .bind(id).bind(j).bind(owner.workspace_id).execute(&pool).await.unwrap();
            }
        } else {
            what = "move or delete an unreserved execution";
            let orphan: Option<Uuid> = sqlx::query_scalar("SELECT id FROM inference_executions e WHERE NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id) AND NOT EXISTS(SELECT 1 FROM async_jobs j WHERE j.execution_id=e.id) ORDER BY id LIMIT 1").fetch_optional(&pool).await.unwrap();
            if let Some(id) = orphan {
                if rng.random_bool(0.5) {
                    sqlx::query("DELETE FROM inference_executions WHERE id=$1")
                        .bind(id)
                        .execute(&pool)
                        .await
                        .unwrap();
                } else {
                    sqlx::query("UPDATE inference_executions SET started_at=started_at+interval '1 minute',workspace_id=$2,api_key_id=$3 WHERE id=$1").bind(id).bind(f.team.workspace_id).bind(f.team.key_id).execute(&pool).await.unwrap();
                }
            }
        }
        if step % 10 == 0 || step >= 295 {
            for at in instants {
                assert_parity(&pool, at, &format!("step {step} after {what}")).await;
            }
        }
    }
    let jobs_live: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(jobs),0)::bigint FROM inflight_counters WHERE scope_kind='workspace'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let inflight: i64 = sqlx::query_scalar("SELECT coalesce(sum(requests),0)::bigint FROM inflight_counters WHERE scope_kind='workspace'").fetch_one(&pool).await.unwrap();
    assert!(
        inflight > 0 && jobs_live > 0,
        "the interleaving should leave live work"
    );
    assert!(
        busy > 0,
        "the interleaving should exercise tokens/minute denials"
    );
}

/// Requests per minute reset exactly at the UTC minute boundary; a lease that
/// expires at the admission instant no longer holds a concurrency slot.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn minute_and_lease_boundaries_on_the_pinned_clock(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let boundary: DateTime<Utc> = sqlx::query_scalar(
        "SELECT date_trunc('minute',clock_timestamp(),'UTC')+interval '1 minute'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let pinned = |at: DateTime<Utc>| {
        let store = Store::new(pool.clone());
        store.pin_admission_clock(at).unwrap();
        store
    };
    let last = boundary - chrono::TimeDelta::microseconds(1);
    let before = pinned(last);
    let after = pinned(boundary);
    // Two requests per minute, at most three at once.
    f.policy("workspace_local_policies", Some(2), None, Some(3), None)
        .await;
    for _ in 0..2 {
        admit(&before, &f.start(), &request(), 60).await.unwrap();
    }
    assert_eq!(
        admit(&before, &f.start(), &request(), 60).await,
        Err(InferenceError::Busy)
    );
    assert_parity(&pool, last, "last microsecond of the minute").await;
    // The next minute starts empty for requests/min; concurrency still counts.
    admit(&after, &f.start(), &request(), 60).await.unwrap();
    assert_eq!(
        admit(&after, &f.start(), &request(), 60).await,
        Err(InferenceError::Busy),
        "three live leases fill requests-at-once"
    );
    assert_parity(&pool, boundary, "first microsecond of the next minute").await;
    let row: (i64, i64) = sqlx::query_as("SELECT requests,(SELECT requests FROM rate_minute_counters WHERE minute_start=$2 AND scope_kind='workspace' AND scope_id=$3) FROM rate_minute_counters WHERE minute_start=$1 AND scope_kind='workspace' AND scope_id=$3")
        .bind(boundary - chrono::TimeDelta::minutes(1)).bind(boundary).bind(f.principal.workspace_id)
        .fetch_one(&pool).await.unwrap();
    assert_eq!(row, (2, 1));
    // Leases of the first two end at `last + 60 s`: at that exact instant they
    // are expired (lease > now is false), one microsecond earlier they hold.
    f.policy("workspace_local_policies", None, None, Some(3), None)
        .await;
    let expiry = last + chrono::TimeDelta::seconds(60);
    let just_before = pinned(expiry - chrono::TimeDelta::microseconds(1));
    assert_eq!(
        admit(&just_before, &f.start(), &request(), 60).await,
        Err(InferenceError::Busy)
    );
    assert_parity(
        &pool,
        expiry - chrono::TimeDelta::microseconds(1),
        "lease live",
    )
    .await;
    let at_expiry = pinned(expiry);
    admit(&at_expiry, &f.start(), &request(), 60).await.unwrap();
    admit(&at_expiry, &f.start(), &request(), 60).await.unwrap();
    assert_eq!(
        admit(&at_expiry, &f.start(), &request(), 60).await,
        Err(InferenceError::Busy)
    );
    assert_parity(&pool, expiry, "lease expired at the admission instant").await;
}

/// Tokens per minute: observed excess raises consumption, an unreserved
/// execution blocks the minute, jobs and batch lines are exempt.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn tokens_per_minute_match_the_scan(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await; // 110 reserved tokens with max_output_tokens 10
    let now = f.store.freeze_admission_clock().await.unwrap();
    f.policy("workspace_local_policies", None, Some(400), None, None)
        .await;
    let a = f.start();
    admit(&f.store, &a, &request(), 60).await.unwrap();
    // Settled with 250 observed tokens: 250 + 110 <= 400 fits, 250+110+110 does not.
    finish(&f.store, &done(a.id, Some(100), Some(150)))
        .await
        .unwrap();
    admit(&f.store, &f.start(), &request(), 60).await.unwrap();
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 60).await,
        Err(InferenceError::Busy)
    );
    assert_parity(&pool, now, "observed excess").await;
    // An async video job's reservation does not count toward tokens/minute.
    let e = insert_execution(&pool, &f, f.principal, "videos", now, None).await;
    insert_reservation(
        &pool,
        &f,
        f.principal,
        e,
        sqlx::query_scalar("SELECT date_trunc('minute',$1::timestamptz,'UTC')")
            .bind(now)
            .fetch_one(&pool)
            .await
            .unwrap(),
        now + chrono::TimeDelta::seconds(60),
        Some(10_000),
    )
    .await;
    assert_parity(&pool, now, "job exempt").await;
    // An execution without a reservation this minute makes the sum a lower bound.
    f.policy("workspace_local_policies", None, Some(100_000), None, None)
        .await;
    admit(&f.store, &f.start(), &request(), 60).await.unwrap();
    insert_execution(&pool, &f, f.principal, "generation", now, None).await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 60).await,
        Err(InferenceError::Busy)
    );
    assert_parity(&pool, now, "unreserved execution").await;
}

/// The migration backfills exactly from existing history (pending and live
/// leases, async jobs, batch lines, unreserved executions, old minutes) and
/// splits unknown holds out of the budget totals.
#[sqlx::test(migrations = false)]
async fn migrations_backfill_existing_history_exactly(pool: PgPool) {
    let before = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version < 24)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&pool).await.unwrap();
    sqlx::raw_sql(r#"DO $$ DECLARE u uuid:=gen_random_uuid(); ws uuid:=gen_random_uuid(); k uuid:=gen_random_uuid(); k2 uuid:=gen_random_uuid(); m uuid:=gen_random_uuid(); pc uuid:=gen_random_uuid(); d uuid:=gen_random_uuid(); e uuid; j uuid; i integer; t timestamptz:=date_trunc('minute',clock_timestamp(),'UTC'); BEGIN
     INSERT INTO users(id,email) VALUES(u,'backfill@test.invalid');
     INSERT INTO workspaces(id,name,kind) VALUES(ws,'Backfill','team');
     INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,ws,u,'a',decode(repeat('01',32),'hex'));
     INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES(k2,ws,u,'b',decode(repeat('02',32),'hex'),k);
     INSERT INTO models(id,public_name) VALUES(m,'backfill');
     INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES(pc,'c','openai','env:X');
     INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES(d,m,pc,'x');
     FOR i IN 1..80 LOOP
      e:=gen_random_uuid();
      INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,workload_kind)
       VALUES(e,ws,CASE WHEN i%2=0 THEN k ELSE k2 END,d,'backfill','openai',false,'started',e,t-make_interval(mins=>i%13),CASE i%5 WHEN 0 THEN 'videos' WHEN 1 THEN 'batches' ELSE 'generation' END);
      IF i%7<>0 THEN
       INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost,reserved_tokens,input_tokens,output_tokens,actual_microusd)
        VALUES(e,ws,CASE WHEN i%2=0 THEN k ELSE k2 END,d,t-make_interval(mins=>i%13),t-make_interval(mins=>i%13),date_trunc('month',t),t+make_interval(secs=>(i%9-3)*10),
         CASE i%3 WHEN 0 THEN 'settled' WHEN 1 THEN 'unknown' ELSE 'pending' END,
         CASE WHEN i%11=0 THEN NULL ELSE 100+i END,i%4=0,CASE WHEN i%6=0 THEN NULL ELSE 50+i END,
         CASE WHEN i%3=0 THEN 30+i END,CASE WHEN i%3=0 THEN 2*i END,CASE WHEN i%3=0 THEN i END);
       IF i%5 IN (0,1) AND i%2=0 THEN
        j:=gen_random_uuid();
        INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,video_seconds,video_size,batch_endpoint,state)
         VALUES(j,CASE WHEN i%5=0 THEN 'video' ELSE 'batch' END,ws,k,d,e,'backfill','openai','up-'||i,t+interval '1 day',CASE WHEN i%5=0 THEN 4 END,CASE WHEN i%5=0 THEN '1280x720' END,CASE WHEN i%5=1 THEN '/v1/chat/completions' END,CASE WHEN i%4=0 THEN 'completed' ELSE 'queued' END);
       END IF;
      END IF;
     END LOOP; END $$"#).execute(&pool).await.unwrap();
    crate::store::MIGRATOR.run(&pool).await.unwrap();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    for at in [now, now - chrono::TimeDelta::minutes(2)] {
        assert_parity(&pool, at, "backfill").await;
    }
    let split: (String, String) = sqlx::query_as("SELECT sum(held_unknown_microusd)::text,(SELECT coalesce(sum(held_microusd),0) FROM governance_reservations WHERE state='unknown')::text FROM budget_totals WHERE scope_kind='workspace' AND period='lifetime'").fetch_one(&pool).await.unwrap();
    assert_eq!(split.0, split.1);
    assert_ne!(split.0, "0");
}

/// Expiry reconciliation works in SKIP LOCKED batches: concurrent callers
/// never process a row twice, holds are retained, counters release slots.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batched_expiry_reconciliation_is_exact_and_concurrent_safe(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    for _ in 0..130 {
        admit(&f.store, &f.start(), &request(), 3600).await.unwrap();
    }
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let (a, b, c) = tokio::join!(
        reconcile_expired(&f.store, 60),
        reconcile_expired(&f.store, 60),
        reconcile_expired(&f.store, 60)
    );
    let first = a.unwrap() + b.unwrap() + c.unwrap();
    let rest = reconcile_expired(&f.store, 1000).await.unwrap();
    assert_eq!(first + rest, 130);
    assert_eq!(reconcile_expired(&f.store, 10).await.unwrap(), 0);
    let row: (i64, i64, String) = sqlx::query_as("SELECT count(*) FILTER(WHERE state='unknown'),(SELECT count(*) FROM monetary_ledger WHERE kind='unknown'),coalesce(sum(held_microusd),0)::text FROM governance_reservations").fetch_one(&pool).await.unwrap();
    assert_eq!(row, (130, 130, (130 * 110).to_string()));
    let cancelled: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE state='cancelled' AND error_code='lease_expired'").fetch_one(&pool).await.unwrap();
    assert_eq!(cancelled, 130);
    let inflight: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(requests),0)::bigint FROM inflight_counters WHERE scope_kind='workspace'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(inflight, 0);
    let mut tx = pool.begin().await.unwrap();
    let report = totals::verify_in(&mut tx).await.unwrap();
    assert!(report.consistent(), "{report:#?}");
    // Unknown cost retains its hold in the totals and in the unknown split.
    let held: (String, String) = sqlx::query_as("SELECT sum(held_microusd)::text,sum(held_unknown_microusd)::text FROM budget_totals WHERE scope_kind='workspace' AND period='lifetime'").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(held, ((130 * 110).to_string(), (130 * 110).to_string()));
}

/// Pruning removes only minutes admission no longer reads; the guard refuses
/// removing retained minutes even for the runtime's own DELETE.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn pruning_keeps_the_retained_window(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    admit(&f.store, &f.start(), &request(), 60).await.unwrap();
    sqlx::query("INSERT INTO rate_minute_counters(minute_start,scope_kind,scope_id,requests) VALUES(date_trunc('minute',now(),'UTC')-interval '11 minutes','workspace',$1,5),(date_trunc('minute',now(),'UTC')-interval '30 minutes','workspace',$1,5)")
        .bind(f.principal.workspace_id).execute(&pool).await.unwrap();
    assert_eq!(rates::prune(&f.store, 1000).await.unwrap(), 2);
    assert_eq!(rates::prune(&f.store, 1000).await.unwrap(), 0);
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM rate_minute_counters")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 2);
    assert!(
        sqlx::query("DELETE FROM rate_minute_counters")
            .execute(&pool)
            .await
            .is_err()
    );
}

/// The one-statement admission revalidation equals the multi-statement form
/// for every live-authority state (key, workspace, user, roles, membership,
/// service account, personal ownership).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn admission_revalidation_matches_the_multi_statement_form(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let account = Uuid::new_v4();
    let service_key = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'bot')")
        .bind(account)
        .bind(f.team.workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'svc',decode(repeat('07',32),'hex'),$1)")
        .bind(service_key).bind(f.team.workspace_id).bind(account).execute(&pool).await.unwrap();
    let member_key = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'member',decode(repeat('08',32),'hex'),$1)")
        .bind(member_key).bind(f.team.workspace_id).bind(f.other).execute(&pool).await.unwrap();
    let principals = [
        f.principal,
        f.team,
        Principal {
            key_id: service_key,
            workspace_id: f.team.workspace_id,
            user_id: None,
        },
        Principal {
            key_id: member_key,
            workspace_id: f.team.workspace_id,
            user_id: Some(f.other),
        },
        // Mismatched claims: another user, or a user on a service key.
        Principal {
            user_id: Some(f.other),
            ..f.principal
        },
        Principal {
            key_id: service_key,
            workspace_id: f.team.workspace_id,
            user_id: Some(f.owner),
        },
        Principal {
            key_id: member_key,
            workspace_id: f.principal.workspace_id,
            user_id: Some(f.other),
        },
    ];
    let compare = |context: &'static str| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            let mut allowed = 0;
            for p in principals {
                let old = crate::auth::revalidate(&mut tx, &p).await.unwrap();
                let new = crate::auth::revalidate_admission(&mut tx, &p)
                    .await
                    .unwrap();
                assert_eq!(old, new, "{context}: {p:?}");
                allowed += usize::from(new.is_some());
            }
            tx.rollback().await.unwrap();
            allowed
        }
    };
    assert_eq!(compare("baseline").await, 4);
    let changes: [(&'static str, String); 9] = [
        (
            "membership revoked",
            format!(
                "UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id='{}'",
                f.other
            ),
        ),
        (
            "service account disabled",
            format!("UPDATE service_accounts SET disabled_at=now() WHERE id='{account}'"),
        ),
        (
            "key disabled",
            format!(
                "UPDATE api_keys SET disabled_at=now() WHERE id='{}'",
                f.team.key_id
            ),
        ),
        (
            "key expired",
            format!(
                "UPDATE api_keys SET expires_at=now()-interval '1 second' WHERE id='{}'",
                f.principal.key_id
            ),
        ),
        (
            "roles revoked",
            format!(
                "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id='{}'",
                f.other
            ),
        ),
        (
            "user disabled",
            format!("UPDATE users SET disabled_at=now() WHERE id='{}'", f.owner),
        ),
        (
            "workspace disabled",
            format!(
                "UPDATE workspaces SET disabled_at=now() WHERE id='{}'",
                f.team.workspace_id
            ),
        ),
        (
            "key revoked",
            format!("UPDATE api_keys SET revoked_at=now() WHERE id='{service_key}'"),
        ),
        (
            "user cleaned",
            format!("UPDATE users SET cleaned_at=now() WHERE id='{}'", f.other),
        ),
    ];
    for (context, sql) in changes {
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        compare(context).await;
    }
    assert_eq!(compare("everything revoked").await, 0);
}
