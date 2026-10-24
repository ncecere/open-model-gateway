//! #101 C: database-backed alert behaviour, independent of SMTP (slice D).
// Exact band/cap tests below call the tick with a captured DATABASE timestamp,
// just like the key-expiry tests; production must obtain it after fencing.
use super::*;
use sqlx::PgPool;

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
        token: Uuid::new_v4(),
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
