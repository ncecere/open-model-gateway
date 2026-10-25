//! #102C composes with #277 after the full migration prefix, on real PostgreSQL.
use super::{
    generation::{GENERATE, generation_events},
    *,
};
use crate::leases::{Lease, Leases};

// Publish valid immutable runs/snapshots without enqueuing, so direct SQL can
// exercise the INSERT guards independently of the scheduler's recipient cap.
async fn report_runs(s: &Seed, workspace: Option<Uuid>, audience: i32, n: usize) -> Vec<Uuid> {
    let leases = Leases::new();
    leases.renew_once(&s.pool).await;
    let fence = leases.held(Lease::ReportScheduler).unwrap();
    let mut runs = Vec::new();
    for _ in 0..n {
        let schedule: Uuid = sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,workspace_id,created_by,cadence,timezone,delivery_time,state) VALUES($1,$2,$3,'weekly','UTC','00:01','active') RETURNING id")
            .bind(if workspace.is_some() { "workspace" } else { "installation" }).bind(workspace).bind(s.owner).fetch_one(&s.pool).await.unwrap();
        for slot in 1..=audience {
            let user = Uuid::new_v4();
            sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
                .bind(user)
                .bind(format!("{user}@report.test"))
                .execute(&s.pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,$2,$3)",
            )
            .bind(schedule)
            .bind(slot)
            .bind(user)
            .execute(&s.pool)
            .await
            .unwrap();
        }
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO report_generation_claims(schedule_id,token,configuration_revision,period_start,period_end,skipped_intervals,expires_at) SELECT id,gen_random_uuid(),configuration_revision,date_trunc('week',clock_timestamp(),'UTC')-interval '7 days',date_trunc('week',clock_timestamp(),'UTC'),0,clock_timestamp()+interval '1 minute' FROM report_schedules WHERE id=$1")
            .bind(schedule).execute(&mut *tx).await.unwrap();
        let run: Uuid = sqlx::query_scalar("INSERT INTO report_runs(schedule_id,period_start,period_end,configuration_revision,claim_token,lease_holder,lease_epoch,skipped_intervals,observed_at) SELECT schedule_id,period_start,period_end,configuration_revision,token,$2,$3,skipped_intervals,clock_timestamp() FROM report_generation_claims WHERE schedule_id=$1 RETURNING id")
            .bind(schedule).bind(fence.holder).bind(fence.epoch).fetch_one(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO report_snapshots(run_id,body) VALUES($1,'{}')")
            .bind(run)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        runs.push(run);
    }
    runs
}
const REPORT_ENVELOPE: &str = "INSERT INTO alert_deliveries(id,report_run_id,transition) VALUES(gen_random_uuid(),$1,'fired') RETURNING id";
const REPORT_RECIPIENTS: &str = "INSERT INTO alert_recipient_outbox(delivery_id,fingerprint,internal_user_id) SELECT d.id,sha256(uuid_send(d.id)||convert_to(':omg.alert.recipient.v1:','UTF8')||convert_to(lower(btrim(u.email)),'UTF8')),u.id FROM alert_deliveries d JOIN report_runs r ON r.id=d.report_run_id JOIN users u ON r.configuration->'recipient_user_ids' @> jsonb_build_array(u.id) WHERE d.id=$1 ORDER BY u.id";

#[sqlx::test(migrations = false)]
async fn scheduled_reports_delivery_upgrade_preserves_generation_functions_and_history(
    pool: PgPool,
) {
    let all = sqlx::migrate!("./enterprise_migrations");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version < 65).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let s = seed(pool).await;
    let functions = "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE pronamespace='public'::regnamespace AND proname IN ('alert_delivery_set_bucket','alert_delivery_lock_buckets','alert_delivery_recent_count','alert_delivery_generation_immutable') ORDER BY proname";
    let before: Vec<String> = sqlx::query_scalar(functions)
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(before.len(), 4);
    let ids = generation_events(&s, Some(s.team), 101).await;
    sqlx::query(GENERATE)
        .bind(&ids)
        .execute(&s.pool)
        .await
        .unwrap();
    s.store.migrate_enterprise().await.unwrap();
    let after: Vec<String> = sqlx::query_scalar(functions)
        .fetch_all(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "0065 must not replace #277's generation admission"
    );
    let counts: (i64,i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE quota_charged),count(*) FILTER(WHERE status='suppressed' AND suppression_reason='workspace_generation_limit') FROM alert_deliveries").fetch_one(&s.pool).await.unwrap();
    assert_eq!(counts, (100, 1));
    assert!(sqlx::query("UPDATE alert_deliveries SET status='pending',completed_at=NULL WHERE status='suppressed'").execute(&s.pool).await.is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_quota_exempt_alerts_still_charged_and_suppressed(pool: PgPool) {
    let s = seed(pool).await;
    sqlx::query("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('test','https://receiver.invalid/','env:HOOK')").execute(&s.pool).await.unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true")
        .execute(&s.pool)
        .await
        .unwrap();
    // More than a full day's alert allowance in the exact same workspace
    // bucket must neither consume it nor be suppressed by it.
    let runs = report_runs(&s, Some(s.team), 1, 102).await;
    sqlx::query("INSERT INTO alert_deliveries(id,report_run_id,transition,bucket_id,created_at,quota_charged,suppression_reason) SELECT gen_random_uuid(),id,'fired',$2,'2000-01-01',true,'workspace_generation_limit' FROM unnest($1::uuid[]) id")
        .bind(&runs[..101]).bind(s.personal).execute(&s.pool).await.unwrap();
    let ids = generation_events(&s, Some(s.team), 101).await;
    sqlx::query(GENERATE)
        .bind(&ids)
        .execute(&s.pool)
        .await
        .unwrap();
    // Even at alert capacity a new report still publishes.
    sqlx::query(REPORT_ENVELOPE)
        .bind(runs[101])
        .execute(&s.pool)
        .await
        .unwrap();
    let counts: (i64,i64,i64,i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE report_run_id IS NOT NULL AND status='report_pending' AND NOT quota_charged AND suppression_reason IS NULL AND bucket_id=$1 AND created_at>clock_timestamp()-interval '1 hour'),count(*) FILTER(WHERE event_id IS NOT NULL AND quota_charged AND status='pending'),count(*) FILTER(WHERE event_id IS NOT NULL AND NOT quota_charged AND status='suppressed' AND suppression_reason='workspace_generation_limit'),(SELECT count(*) FROM alert_webhook_outbox) FROM alert_deliveries")
        .bind(s.team).fetch_one(&s.pool).await.unwrap();
    assert_eq!(counts, (102, 100, 1, 101));
    let charged: i64 =
        sqlx::query_scalar("SELECT alert_delivery_recent_count($1,clock_timestamp())")
            .bind(s.team)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(charged, 100);
    assert!(
        sqlx::query("UPDATE alert_deliveries SET quota_charged=true WHERE report_run_id=$1")
            .bind(runs[0])
            .execute(&s.pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_recipient_guard_50_51_and_stale_isolation(pool: PgPool) {
    let s = seed(pool).await;
    let run = report_runs(&s, None, 50, 1).await[0];
    let delivery: Uuid = sqlx::query_scalar(REPORT_ENVELOPE)
        .bind(run)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let mut stale_read = s.pool.begin().await.unwrap();
    let mut stale_serial = s.pool.begin().await.unwrap();
    for (tx, isolation) in [
        (&mut stale_read, "REPEATABLE READ"),
        (&mut stale_serial, "SERIALIZABLE"),
    ] {
        sqlx::query(&format!("SET TRANSACTION ISOLATION LEVEL {isolation}"))
            .execute(&mut **tx)
            .await
            .unwrap();
        sqlx::query("SELECT count(*) FROM alert_recipient_outbox")
            .execute(&mut **tx)
            .await
            .unwrap();
    }
    // One multi-row statement must still count earlier inserted recipients.
    assert_eq!(
        sqlx::query(REPORT_RECIPIENTS)
            .bind(delivery)
            .execute(&s.pool)
            .await
            .unwrap()
            .rows_affected(),
        50
    );
    let error = sqlx::query(REPORT_RECIPIENTS)
        .bind(delivery)
        .execute(&s.pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    assert!(
        error
            .as_database_error()
            .unwrap()
            .message()
            .contains("maximum fifty")
    );
    for mut tx in [stale_read, stale_serial] {
        let error = sqlx::query(REPORT_RECIPIENTS)
            .bind(delivery)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("25001")
        );
        tx.rollback().await.unwrap();
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM alert_recipient_outbox WHERE delivery_id=$1")
            .bind(delivery)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(count, 50);
    sqlx::query("UPDATE alert_deliveries SET initialized=true WHERE id=$1")
        .bind(delivery)
        .execute(&s.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query(REPORT_RECIPIENTS)
            .bind(delivery)
            .execute(&s.pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_envelope_isolation_and_recipient_lineage(pool: PgPool) {
    let s = seed(pool).await;
    let run = report_runs(&s, Some(s.team), 1, 1).await[0];
    for isolation in ["REPEATABLE READ", "SERIALIZABLE"] {
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query(&format!("SET TRANSACTION ISOLATION LEVEL {isolation}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let error = sqlx::query(REPORT_ENVELOPE)
            .bind(run)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("25001")
        );
        tx.rollback().await.unwrap();
    }
    let delivery: Uuid = sqlx::query_scalar(REPORT_ENVELOPE)
        .bind(run)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let error = sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint,internal_user_id) VALUES($1,sha256('forged'::bytea),$2)").bind(delivery).bind(s.owner).execute(&s.pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    assert_eq!(
        sqlx::query(REPORT_RECIPIENTS)
            .bind(delivery)
            .execute(&s.pool)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
}
