//! #102C RED contracts. Real PostgreSQL and the existing loopback SMTP fixture.
use super::*;
use crate::{
    email::mock::{self, Behaviour},
    leases::{Lease, Leases},
};
use sqlx::PgPool;

async fn published(pool: &PgPool, kind: &str) -> (Store, Uuid, Uuid, Uuid, Option<Uuid>) {
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'report@example.test')")
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(user)
    .execute(pool)
    .await
    .unwrap();
    let ws = if kind == "installation" {
        None
    } else {
        let ws = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'PRIVATE WORKSPACE',$2,$3)")
            .bind(ws).bind(if kind == "personal" { "personal" } else { "team" }).bind(if kind == "personal" { Some(user) } else { None }).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'owner','manual')").bind(ws).bind(user).execute(pool).await.unwrap();
        Some(ws)
    };
    let schedule: Uuid = sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,workspace_id,created_by,cadence,timezone,delivery_time,state) VALUES($1,$2,$3,'weekly','UTC','00:01','active') RETURNING id").bind(kind).bind(ws).bind(user).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,1,$2)")
        .bind(schedule)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',clock_timestamp(),'UTC')-interval '7 days',next_run_at=clock_timestamp()-interval '1 day' WHERE id=$1").bind(schedule).execute(pool).await.unwrap();
    let leases = Leases::new();
    leases.renew_once(pool).await;
    let f = leases.held(Lease::ReportScheduler).unwrap();
    let store = Store::new(pool.clone());
    let c = crate::scheduled_reports::claim(&store, &f, schedule)
        .await
        .unwrap()
        .unwrap();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(
        crate::scheduled_reports::complete(
            &store,
            &f,
            &c,
            &json!({"observed_at":now,"settled_microusd":"987654321","private":"PRIVATE WORKSPACE"})
        )
        .await
        .unwrap()
    );
    let run = sqlx::query_scalar("SELECT id FROM report_runs WHERE schedule_id=$1")
        .bind(schedule)
        .fetch_one(pool)
        .await
        .unwrap();
    (store, schedule, run, user, ws)
}
async fn configured(pool: &PgPool) {
    sqlx::query("UPDATE installation_settings SET smtp_host='127.0.0.1',smtp_port=2525,smtp_tls='none',smtp_from_address='gateway@example.test' WHERE singleton").execute(pool).await.unwrap();
}
async fn report_claim(store: &Store) -> Option<Claim> {
    claim_with_origin(
        store,
        Uuid::new_v4(),
        &HashMap::new(),
        std::future::ready(()),
        Some("https://gateway.example.test".into()),
    )
    .await
    .unwrap()
    .unwrap()
    .1
}
async fn state(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT state FROM alert_recipient_outbox")
        .fetch_one(pool)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_enqueue_idempotent_and_lineage(pool: PgPool) {
    let (_, _, run, _, _) = published(&pool, "installation").await;
    let mut tx = pool.begin().await.unwrap();
    crate::scheduled_reports::delivery::enqueue(&mut tx, run)
        .await
        .unwrap();
    crate::scheduled_reports::delivery::enqueue(&mut tx, run)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (count, initial, old_visible): (i64, bool, i64) = sqlx::query_as("SELECT count(*),bool_and(initialized),(SELECT count(*) FROM alert_deliveries WHERE status='pending') FROM alert_deliveries WHERE report_run_id=$1 AND event_id IS NULL").bind(run).fetch_one(&pool).await.unwrap();
    assert_eq!((count, initial, old_visible), (1, true, 0));
    assert_eq!(state(&pool).await, "waiting");
    assert!(
        sqlx::query("UPDATE alert_deliveries SET report_run_id=gen_random_uuid()")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE alert_recipient_outbox SET internal_user_id=NULL")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO alert_deliveries(id,event_id,report_run_id,transition) VALUES(gen_random_uuid(),NULL,gen_random_uuid(),'fired')").execute(&pool).await.is_err());
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_live_refusals_never_redirect_or_reactivate(pool: PgPool) {
    let (store, schedule, _, user, _) = published(&pool, "installation").await;
    configured(&pool).await;
    let first = report_claim(&store).await.unwrap();
    assert!(
        complete(&store, &first, Err(DeliveryError::Connection))
            .await
            .unwrap()
    );
    sqlx::query("UPDATE users SET email='changed@example.test' WHERE id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(report_claim(&store).await.is_none());
    assert_eq!(state(&pool).await, "ineligible");
    sqlx::query("UPDATE users SET email='report@example.test' WHERE id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(deliver_pending(&store, 25).await, 0);
    assert_eq!(state(&pool).await, "ineligible");
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE resource_id=$1 AND action='report.delivery_outcome'").bind(schedule).fetch_one(&pool).await.unwrap();
    assert!(audits > 0);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_revoked_role(pool: PgPool) {
    let (store, _, _, user, _) = published(&pool, "installation").await;
    configured(&pool).await;
    let first = report_claim(&store).await.unwrap();
    assert!(
        complete(&store, &first, Err(DeliveryError::Connection))
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE user_id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    assert!(report_claim(&store).await.is_none());
    assert_eq!(state(&pool).await, "ineligible");
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_removed_membership(pool: PgPool) {
    let (store, _, _, user, ws) = published(&pool, "workspace").await;
    configured(&pool).await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE user_id=$1 AND workspace_id=$2")
        .bind(user)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    assert!(report_claim(&store).await.is_none());
    assert_eq!(state(&pool).await, "ineligible");
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_suspended_user(pool: PgPool) {
    let (store, _, _, user, _) = published(&pool, "personal").await;
    configured(&pool).await;
    sqlx::query("UPDATE users SET disabled_at=clock_timestamp() WHERE id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    assert!(report_claim(&store).await.is_none());
    assert_eq!(state(&pool).await, "ineligible");
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_retry_backoff_expiry_fencing_and_not_configured(pool: PgPool) {
    let (store, _, _, _, _) = published(&pool, "installation").await;
    configured(&pool).await;
    let first = report_claim(&store).await.unwrap();
    assert!(
        complete(&store, &first, Err(DeliveryError::Connection))
            .await
            .unwrap()
    );
    let backoff: bool = sqlx::query_scalar("SELECT next_attempt_at>clock_timestamp()+interval '50 seconds' FROM alert_recipient_outbox").fetch_one(&pool).await.unwrap();
    assert!(backoff);
    assert!(
        claim_with_origin(
            &store,
            Uuid::new_v4(),
            &HashMap::new(),
            std::future::ready(()),
            Some("https://gateway.example.test".into())
        )
        .await
        .unwrap()
        .is_none()
    );
    sqlx::query(
        "UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let stale = report_claim(&store).await.unwrap();
    sqlx::query(
        "UPDATE alert_recipient_outbox SET lease_expires_at=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let current = report_claim(&store).await.unwrap();
    assert!(!complete(&store, &stale, Ok(())).await.unwrap());
    assert!(
        complete(&store, &current, Err(DeliveryError::Connection))
            .await
            .unwrap()
    );
    sqlx::query("UPDATE installation_settings SET smtp_host=NULL WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    deliver_pending(&store, 25).await;
    assert_eq!(state(&pool).await, "not_configured");
    assert!(!complete(&store, &current, Ok(())).await.unwrap());
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_five_attempts_exhaust_without_replay(pool: PgPool) {
    let (store, _, _, _, _) = published(&pool, "installation").await;
    configured(&pool).await;
    for attempt in 1..=5 {
        let c = report_claim(&store).await.unwrap();
        assert!(
            complete(&store, &c, Err(DeliveryError::Connection))
                .await
                .unwrap()
        );
        let n: i32 = sqlx::query_scalar("SELECT attempt_count FROM alert_recipient_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, attempt);
        if attempt < 5 {
            assert_eq!(state(&pool).await, "waiting");
            sqlx::query("UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        }
    }
    assert_eq!(state(&pool).await, "exhausted");
    assert_eq!(deliver_pending(&store, 25).await, 0);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_24h_cleanup_and_initial_not_configured(pool: PgPool) {
    let (store, _, _, _, _) = published(&pool, "installation").await;
    assert_eq!(deliver_pending(&store, 25).await, 1);
    assert_eq!(state(&pool).await, "not_configured");
    assert_eq!(deliver_pending(&store, 25).await, 0);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_expired_lease_24h_is_fenced(pool: PgPool) {
    let (store, _, _, _, _) = published(&pool, "installation").await;
    configured(&pool).await;
    let c = report_claim(&store).await.unwrap();
    // Owner-only clock fixture, as in #277's alert outbox tests. Production
    // callers cannot rewrite the immutable generation timestamp.
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql("ALTER TABLE alert_deliveries DISABLE TRIGGER alert_delivery_generation_immutable; UPDATE alert_deliveries SET created_at=clock_timestamp()-interval '25 hours'; ALTER TABLE alert_deliveries ENABLE TRIGGER alert_delivery_generation_immutable")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    deliver_pending(&store, 25).await;
    assert_eq!(state(&pool).await, "exhausted");
    assert!(!complete(&store, &c, Ok(())).await.unwrap());
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_paused_schedule_suppresses(pool: PgPool) {
    let (store, schedule, _, _, _) = published(&pool, "workspace").await;
    configured(&pool).await;
    sqlx::query("UPDATE report_schedules SET state='paused' WHERE id=$1")
        .bind(schedule)
        .execute(&pool)
        .await
        .unwrap();
    assert!(report_claim(&store).await.is_none());
    assert_eq!(state(&pool).await, "ineligible");
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_disabled_workspace_suppresses(pool: PgPool) {
    let (store, _, _, _, ws) = published(&pool, "workspace").await;
    configured(&pool).await;
    sqlx::query("UPDATE workspaces SET disabled_at=clock_timestamp() WHERE id=$1")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    assert!(report_claim(&store).await.is_none());
    assert_eq!(state(&pool).await, "ineligible");
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_delivery_smtp_link_only_and_shared_fair_bucket(pool: PgPool) {
    let (store, schedule, run, _, ws) = published(&pool, "workspace").await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    configured(&pool).await;
    sqlx::query("UPDATE installation_settings SET smtp_port=$1 WHERE singleton")
        .bind(i32::from(port))
        .execute(&pool)
        .await
        .unwrap();
    // An alert in a different bucket competes in exactly the same selection.
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES($1,'scim_last_admin','scim_last_admin','fair',80,'warning','Alert')").bind(event).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    let mut served = HashMap::new();
    served.insert(Uuid::nil(), 1);
    let (_, c, bucket) = claim_with_origin(
        &store,
        Uuid::new_v4(),
        &served,
        std::future::ready(()),
        Some("https://gateway.example.test".into()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(Some(bucket), ws);
    let c = c.unwrap();
    let result = c
        .settings
        .send(
            &store.smtp_secrets,
            OutgoingEmail {
                to: c.email.to.clone(),
                subject: c.email.subject.clone(),
                body: c.email.body.clone(),
            },
        )
        .await;
    assert!(result.is_ok());
    assert!(complete(&store, &c, result).await.unwrap());
    let email = rx.recv().await.unwrap();
    let link = format!(
        "https://gateway.example.test/api/v1/workspaces/{}/report-schedules/{schedule}/runs/{run}",
        ws.unwrap()
    );
    assert!(email.data.contains(&link), "{}", email.data);
    assert!(email.data.contains("UTC"));
    for forbidden in [
        "987654321",
        "PRIVATE WORKSPACE",
        "attachment",
        "text/csv",
        "admin@example.test",
    ] {
        assert!(!email.data.contains(forbidden));
    }
    assert!(email.data.len() < 16 * 1024);
    assert_eq!(email.rcpt_to.len(), 1);
    assert!(rx.try_recv().is_err());
    let mut served = HashMap::new();
    served.insert(bucket, 1);
    let (_, _, next) = claim(&store, Uuid::new_v4(), &served)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next, Uuid::nil());
}
