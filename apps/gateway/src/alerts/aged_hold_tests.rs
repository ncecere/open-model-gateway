//! #101 C: database-backed alert behaviour, independent of SMTP (slice D).
// Exact band/cap tests below call the tick with a captured DATABASE timestamp,
// just like the key-expiry tests; production must obtain it after fencing.
use super::*;
use sqlx::PgPool;

use super::generation::{GENERATE, generation_events, generation_rule};

#[path = "aged_hold_email_tests.rs"]
mod email;
#[path = "aged_hold_metrics_tests.rs"]
mod metrics;
#[path = "aged_hold_reconciliation_tests.rs"]
mod reconciliation;

async fn tick(s: &Seed, now: DateTime<Utc>) -> Changes {
    let mut tx = s.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(EVALUATION_LOCK)
        .execute(&mut *tx)
        .await
        .unwrap();
    let (result, _) = aged_hold_tick(&mut tx, now).await.unwrap();
    tx.commit().await.unwrap();
    result
}

async fn hold_since(s: &Seed, ws: Uuid, key: Uuid, since: DateTime<Utc>) {
    attempt(s, ws, key, "failed", None, "unknown", None, 12000).await;
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,created_at) SELECT gen_random_uuid(),r.execution_id,r.admitted_at,'unknown',$2 FROM governance_reservations r WHERE r.workspace_id=$1 AND r.state='unknown' AND NOT EXISTS(SELECT 1 FROM monetary_ledger l WHERE l.execution_id=r.execution_id AND l.admitted_at=r.admitted_at AND l.kind='unknown')")
        .bind(ws).bind(since).execute(&s.pool).await.unwrap();
}

async fn generation_project(s: &Seed) -> (Uuid, Uuid) {
    let (workspace, key) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Generation project','project')")
        .bind(workspace)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'sentinel',decode(repeat('39',32),'hex'))")
        .bind(key).bind(workspace).bind(s.owner).execute(&s.pool).await.unwrap();
    (workspace, key)
}

/// Exercise the producer bodies without EVALUATION_LOCK: that outer lock
/// normally serializes jobs, but must not hide a bucket-order regression.
/// The trigger makes this RED deterministically before the union prelock,
/// regardless of which concurrent task the scheduler happens to run first.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_generation_prelocks_before_retirement_and_opposite_rule_order(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    let (project, project_key) = generation_project(&s).await;
    let buckets: Vec<Uuid> =
        sqlx::query_scalar("SELECT b FROM unnest($1::uuid[]) b ORDER BY omg_scope_key(b)")
            .bind(vec![s.team, project, Uuid::nil()])
            .fetch_all(&s.pool)
            .await
            .unwrap();
    for (index, bucket) in buckets.iter().enumerate() {
        let (workspace, key) = if bucket.is_nil() {
            (s.personal, s.personal_key)
        } else if *bucket == project {
            (project, project_key)
        } else {
            (s.team, s.key)
        };
        // Higher lock keys are older: aged-hold visits the opposite order
        // from rule reconciliation below, including the anonymous bucket.
        hold_since(
            &s,
            workspace,
            key,
            now - TimeDelta::hours(169) - TimeDelta::seconds(index as i64),
        )
        .await;
    }
    // Escalation must free the existing subject only AFTER the union lock.
    sqlx::query("INSERT INTO alert_events(id,kind,subject_key,workspace_id,level,severity,summary,details,fired_at) VALUES(gen_random_uuid(),'aged_unknown_hold','workspace:'||$1::uuid::text,$1::uuid,1,'warning','Holds with unknown cost are over 24 hours old','{\"threshold_hours\":24}',$2)")
        .bind(s.team).bind(now-TimeDelta::hours(25)).execute(&s.pool).await.unwrap();
    sqlx::raw_sql("CREATE TABLE aged_expected_buckets(bucket uuid PRIMARY KEY); CREATE FUNCTION aged_assert_bucket_union() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT FROM aged_expected_buckets b WHERE NOT EXISTS(SELECT FROM pg_locks l WHERE l.pid=pg_backend_pid() AND l.locktype='advisory' AND l.classid=72419556 AND l.objsubid=2 AND l.granted AND l.objid::bigint=(omg_scope_key(b.bucket)::bigint+4294967296)%4294967296)) THEN RAISE EXCEPTION 'aged producer wrote before locking its complete bucket union'; END IF; RETURN NEW; END $$; CREATE TRIGGER aged_assert_bucket_union BEFORE INSERT OR UPDATE ON alert_events FOR EACH ROW WHEN (NEW.kind='aged_unknown_hold') EXECUTE FUNCTION aged_assert_bucket_union();")
        .execute(&s.pool).await.unwrap();
    sqlx::query("INSERT INTO aged_expected_buckets SELECT * FROM unnest($1::uuid[])")
        .bind(&buckets)
        .execute(&s.pool)
        .await
        .unwrap();
    let rule = generation_rule(&s, None).await;
    let conditions = buckets
        .iter()
        .map(|bucket| Condition {
            subject: bucket.to_string(),
            level: 100,
            critical: true,
            workspace_id: (!bucket.is_nil()).then_some(*bucket),
            connection_id: None,
            summary: "Bucket order test".into(),
            details: json!({}),
        })
        .collect();
    let barrier = tokio::sync::Barrier::new(2);
    let aged = async {
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await
            .unwrap();
        barrier.wait().await;
        let (changes, _) = aged_hold_tick(&mut tx, now).await.unwrap();
        tx.commit().await.unwrap();
        changes
    };
    let rules = async {
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await
            .unwrap();
        barrier.wait().await;
        let changes = reconcile(&mut tx, Source::Rule(rule), Kind::Budget, conditions)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        changes
    };
    let (aged, rules) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(aged, rules) })
            .await
            .unwrap();
    assert_eq!(
        aged,
        Changes {
            fired: 3,
            resolved: 1
        }
    );
    assert_eq!(
        rules,
        Changes {
            fired: 3,
            resolved: 0
        }
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE quota_charged"
        )
        .await,
        6
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='superseded'"
        )
        .await,
        1
    );
}

type AgedGenerationRow = (Option<Uuid>, Uuid, String, bool);

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_generation_quota_consumption_suppression_and_personal_nil_bucket(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    let (project, project_key) = generation_project(&s).await;
    for (workspace, n) in [(Some(s.team), 99), (Some(project), 100), (None, 100)] {
        let events = generation_events(&s, workspace, n).await;
        sqlx::query(GENERATE)
            .bind(events)
            .execute(&s.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('test','https://receiver.invalid/','env:HOOK')").execute(&s.pool).await.unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true")
        .execute(&s.pool)
        .await
        .unwrap();
    for (workspace, key) in [
        (s.team, s.key),
        (project, project_key),
        (s.personal, s.personal_key),
    ] {
        hold_since(&s, workspace, key, now - TimeDelta::hours(169)).await;
    }
    assert_eq!(
        tick(&s, now).await,
        Changes {
            fired: 3,
            resolved: 0
        }
    );
    let rows: Vec<AgedGenerationRow> = sqlx::query_as("SELECT e.workspace_id,d.bucket_id,d.status,d.quota_charged FROM alert_events e JOIN alert_deliveries d ON d.event_id=e.id WHERE e.kind='aged_unknown_hold'")
        .fetch_all(&s.pool).await.unwrap();
    assert_eq!(rows.len(), 3, "suppression never removes incidents");
    assert!(rows.contains(&(Some(s.team), s.team, "pending".into(), true)));
    assert!(rows.contains(&(Some(project), project, "suppressed".into(), false)));
    assert!(
        rows.contains(&(None, Uuid::nil(), "suppressed".into(), false)),
        "anonymous aggregate has NULL event workspace, never a personal workspace bucket"
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.kind='aged_unknown_hold' AND d.status='suppressed' AND d.suppression_reason='workspace_generation_limit' AND d.completed_at IS NOT NULL AND d.recipients=0 AND d.attempt_count=0 AND NOT d.initialized").await, 2);
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        0
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_webhook_outbox q JOIN alert_events e ON e.id=q.event_id WHERE e.kind='aged_unknown_hold' AND e.workspace_id IS NULL").await, 1,
        "suppressed aggregate email still enqueues the privacy-projected webhook");
    // The admitted aged event spends the last team slot; another rule loses
    // it, while the personal owner's own bucket remains independent of nil.
    for (workspace, expected) in [(Some(s.team), false), (Some(s.personal), true)] {
        let events = generation_events(&s, workspace, 1).await;
        sqlx::query(GENERATE)
            .bind(&events)
            .execute(&s.pool)
            .await
            .unwrap();
        let charged: bool =
            sqlx::query_scalar("SELECT quota_charged FROM alert_deliveries WHERE event_id=$1")
                .bind(events[0])
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(charged, expected);
    }
    assert_eq!(
        tick(&s, now).await,
        Changes::default(),
        "suppression does not erase incident cooldown/dedup"
    );
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE state='unknown'")
        .execute(&s.pool).await.unwrap();
    assert_eq!(
        tick(&s, now).await,
        Changes {
            fired: 0,
            resolved: 3
        }
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.kind='aged_unknown_hold'").await, 3,
        "retirement remains silent, even at email capacity");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_exact_boundaries_escalation_no_step_down_and_cooldown(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    let us = TimeDelta::microseconds(1);
    hold_since(&s, s.team, s.key, now - TimeDelta::hours(24) + us).await;
    assert_eq!(tick(&s, now).await, Changes::default());
    assert_eq!(tick(&s, now + us).await.fired, 1, "exactly 24h");
    assert_eq!(
        tick(&s, now + TimeDelta::hours(144)).await,
        Changes::default(),
        "168h minus one microsecond"
    );
    assert_eq!(
        tick(&s, now + TimeDelta::hours(144) + us).await,
        Changes {
            fired: 1,
            resolved: 1
        }
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='superseded'"
        )
        .await,
        1
    );
    // Simulate reconciliation of the oldest hold, leaving a younger aged
    // hold. There must never be a step down/re-fire while critical is open.
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE workspace_id=$1").bind(s.team).execute(&s.pool).await.unwrap();
    hold_since(&s, s.team, s.key, now + TimeDelta::hours(120)).await;
    assert_eq!(
        tick(&s, now + TimeDelta::hours(145)).await,
        Changes::default()
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE level=2 AND resolved_at IS NULL"
        )
        .await,
        1
    );
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE state='unknown'").execute(&s.pool).await.unwrap();
    assert_eq!(tick(&s, now + TimeDelta::hours(145)).await.resolved, 1);
    hold_since(&s, s.team, s.key, now + TimeDelta::hours(120)).await;
    assert_eq!(
        tick(&s, now + TimeDelta::hours(146)).await.fired,
        0,
        "resolved events count toward cooldown"
    );
    assert_eq!(
        tick(&s, now + TimeDelta::hours(168) + us).await.fired,
        1,
        "exact rolling 24h releases cooldown"
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_tick_and_rolling_caps_defer_without_losing_candidates(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    for n in 0..505 {
        let (ws, key) = (Uuid::new_v4(), Uuid::new_v4());
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'backlog','project')")
            .bind(ws)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'sentinel',decode(repeat('39',32),'hex'))").bind(key).bind(ws).bind(s.owner).execute(&s.pool).await.unwrap();
        hold_since(
            &s,
            ws,
            key,
            now - TimeDelta::hours(25) - TimeDelta::seconds(n),
        )
        .await;
    }
    for _ in 0..5 {
        assert_eq!(tick(&s, now).await.fired, 100);
    }
    assert_eq!(tick(&s, now).await.fired, 0);
    assert_eq!(tick(&s, now + TimeDelta::hours(24)).await.fired, 5);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE kind='aged_unknown_hold'"
        )
        .await,
        505
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_real_evaluator_fires_deduplicates_and_clears(pool: PgPool) {
    let s = seed(pool).await;
    attempt(&s, s.team, s.key, "failed", None, "unknown", None, 1500).await;
    let before: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_kind,scope_id,period,period_start) FROM budget_totals t")
        .fetch_one(&s.pool).await.unwrap();
    let report = evaluate(&s).await;
    assert_eq!((report.fired, report.failed_rules), (1, 0), "{report:?}");
    assert_eq!(evaluate(&s).await.fired, 0);
    let (kind, level, details): (String, i32, Value) =
        sqlx::query_as("SELECT kind,level,details FROM alert_events")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((kind.as_str(), level), ("aged_unknown_hold", 1));
    assert_eq!(details, json!({"threshold_hours":24}));
    let after: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_kind,scope_id,period,period_start) FROM budget_totals t")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(before, after, "tick must never touch accounting");
    sqlx::query("UPDATE workspaces SET disabled_at=clock_timestamp() WHERE id=$1")
        .bind(s.team)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM governance_reservations WHERE state='unknown'"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_job_is_fenced_and_serialized(pool: PgPool) {
    let s = seed(pool).await;
    attempt(&s, s.team, s.key, "failed", None, "unknown", None, 1500).await;
    let leases = crate::leases::Leases::new();
    assert!(leases.renew_once(&s.pool).await > 0);
    let fence = leases.held(crate::leases::Lease::Alerts).unwrap();
    sqlx::raw_sql(&format!("CREATE FUNCTION aged_assert_guards() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_locks WHERE pid=pg_backend_pid() AND relation='work_leases'::regclass AND mode='RowShareLock' AND granted) OR NOT EXISTS(SELECT 1 FROM pg_locks WHERE pid=pg_backend_pid() AND locktype='advisory' AND classid=0 AND objid={EVALUATION_LOCK} AND objsubid=1 AND granted) OR current_setting('statement_timeout')<>'10s' THEN RAISE EXCEPTION 'missing aged job fence/lock/timeout'; END IF; RETURN NEW; END $$; CREATE TRIGGER aged_assert_guards BEFORE INSERT ON alert_events FOR EACH ROW WHEN (NEW.kind='aged_unknown_hold') EXECUTE FUNCTION aged_assert_guards();"))
        .execute(&s.pool).await.unwrap();
    let (a, b) = tokio::join!(
        evaluate_once_fenced(&s.store, Some(&fence)),
        evaluate_once_fenced(&s.store, Some(&fence))
    );
    let reports = [a.unwrap(), b.unwrap()];
    assert_eq!(reports.iter().flatten().map(|r| r.fired).sum::<usize>(), 1);
    assert_eq!(
        reports
            .iter()
            .flatten()
            .map(|r| r.failed_rules)
            .sum::<usize>(),
        0
    );
    let stale = crate::leases::Fence {
        lease: crate::leases::Lease::Alerts,
        holder: Uuid::new_v4(),
        epoch: 99,
    };
    assert!(crate::leases::is_fenced(
        &evaluate_once_fenced(&s.store, Some(&stale))
            .await
            .unwrap_err()
    ));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_unknown_clock_not_admission_and_excluded_states(pool: PgPool) {
    let s = seed(pool).await;
    for state in ["unknown", "pending", "settled"] {
        attempt(&s, s.team, s.key, "failed", None, state, Some(1), 1500).await;
    }
    // A long-running job that only just became unknown is not aged. This
    // ledger row overrides the much older admission timestamp.
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind) SELECT gen_random_uuid(),execution_id,admitted_at,'unknown' FROM governance_reservations WHERE state='unknown'")
        .execute(&s.pool).await.unwrap();
    assert_eq!(evaluate(&s).await.fired, 0);
    // No ledger entry falls back to admission time; excluded states must
    // remain absent even though their admissions are older than seven days.
    attempt(&s, s.team, s.key, "failed", None, "unknown", None, 1500).await;
    assert_eq!(evaluate(&s).await.fired, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_personal_incident_has_no_workspace_identity(pool: PgPool) {
    let s = seed(pool).await;
    attempt(
        &s,
        s.personal,
        s.personal_key,
        "failed",
        None,
        "unknown",
        Some(9007199254740993),
        11000,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let (subject, ws, level, summary, details): (String, Option<Uuid>, i32, String, Value) =
        sqlx::query_as("SELECT subject_key,workspace_id,level,summary,details FROM alert_events")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(subject, "personal");
    assert_eq!(ws, None);
    assert_eq!(level, 2);
    assert_eq!(details, json!({"threshold_hours":168}));
    for sentinel in [
        s.personal.to_string(),
        s.owner.to_string(),
        "Private key name".into(),
        "owner@example.test".into(),
        "9007199254740993".into(),
    ] {
        assert!(!format!("{summary}{details}").contains(&sentinel));
    }
}
