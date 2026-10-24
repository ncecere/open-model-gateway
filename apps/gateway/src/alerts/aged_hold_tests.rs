//! #101 C: database-backed alert behaviour, independent of SMTP (slice D).
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
    let (kind, level, details): (String, i32, Value) = sqlx::query_as("SELECT kind,level,details FROM alert_events")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!((kind.as_str(), level), ("aged_unknown_hold", 1));
    assert_eq!(details, json!({"threshold_hours":24}));
    let after: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_kind,scope_id,period,period_start) FROM budget_totals t")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(before, after, "tick must never touch accounting");
    sqlx::query("UPDATE workspaces SET disabled_at=clock_timestamp() WHERE id=$1").bind(s.team).execute(&s.pool).await.unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'").await, 0);
    assert_eq!(count(&s, "SELECT count(*) FROM governance_reservations WHERE state='unknown'").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_personal_incident_has_no_workspace_identity(pool: PgPool) {
    let s = seed(pool).await;
    attempt(&s, s.personal, s.personal_key, "failed", None, "unknown", Some(9007199254740993), 11000).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let (subject, ws, level, summary, details): (String, Option<Uuid>, i32, String, Value) = sqlx::query_as("SELECT subject_key,workspace_id,level,summary,details FROM alert_events")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(subject, "personal");
    assert_eq!(ws, None);
    assert_eq!(level, 2);
    assert_eq!(details, json!({"threshold_hours":168}));
    for sentinel in [s.personal.to_string(), s.owner.to_string(), "Private key name".into(), "owner@example.test".into(), "9007199254740993".into()] {
        assert!(!format!("{summary}{details}").contains(&sentinel));
    }
}
