//! #101 D: real PostgreSQL recipient claims and loopback SMTP acceptance.
use super::*;

async fn fire(s: &Seed, workspace: Uuid, key: Uuid, hours: i64) {
    let now = expiry_now(s).await;
    hold_since(s, workspace, key, now - TimeDelta::hours(hours)).await;
    assert_eq!(tick(s, now).await.fired, 1);
}

async fn user(s: &Seed, email: &str, role: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(id)
        .bind(email)
        .execute(&s.pool)
        .await
        .unwrap();
    if let Some(role) = role {
        sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')")
            .bind(id)
            .bind(role)
            .execute(&s.pool)
            .await
            .unwrap();
    }
    id
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_recipient_set_is_platform_admin_only_in_every_scope(pool: PgPool) {
    let s = seed(pool).await;
    let auditor = user(&s, "auditor@example.test", Some("auditor")).await;
    let member = user(&s, "member@example.test", Some("user")).await;
    let local_admin = user(&s, "local-admin@example.test", Some("user")).await;
    user(&s, "unentitled@example.test", None).await;
    for (id, role) in [
        (auditor, "admin"),
        (member, "member"),
        (local_admin, "admin"),
    ] {
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')")
            .bind(s.team).bind(id).bind(role).execute(&s.pool).await.unwrap();
    }
    // A former shared admin is also never an email recipient.
    sqlx::query(
        "UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE user_id=$1",
    )
    .bind(local_admin)
    .execute(&s.pool)
    .await
    .unwrap();
    for column in ["disabled_at", "cleaned_at"] {
        let id = user(&s, &format!("{column}@example.test"), Some("admin")).await;
        sqlx::query(&format!(
            "UPDATE users SET {column}=clock_timestamp() WHERE id=$1"
        ))
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
    }
    let (project, project_key) = generation_project(&s).await;
    for (ws, key) in [
        (s.team, s.key),
        (project, project_key),
        (s.personal, s.personal_key),
    ] {
        fire(&s, ws, key, 25).await;
    }
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 3);
    for _ in 0..3 {
        let message = rx.try_recv().expect("one email per incident");
        assert_eq!(message.rcpt_to.len(), 1);
        assert!(message.rcpt_to[0].contains("admin@example.test"));
    }
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='sent' AND recipients=1 AND sent=1"
        )
        .await,
        3
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_personal_aggregate_loopback_acceptance_is_identity_free(pool: PgPool) {
    let s = seed(pool).await;
    fire(&s, s.personal, s.personal_key, 169).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    let message = rx.try_recv().expect("accepted personal aggregate email");
    assert_eq!(message.rcpt_to.len(), 1);
    assert!(message.rcpt_to[0].contains("admin@example.test"));
    assert!(message.data.contains("168 hours"));
    assert!(message.data.contains("Installation"));
    for sentinel in [
        s.personal.to_string(),
        s.owner.to_string(),
        s.personal_key.to_string(),
        "owner@example.test".into(),
        "Private key name".into(),
        "Your personal workspace".into(),
        "Local upstream".into(),
        s.deployment.to_string(),
    ] {
        assert!(
            !message.data.contains(&sentinel),
            "private identifier in email"
        );
    }
    assert!(rx.try_recv().is_err());
    assert_eq!(
        deliver_pending(&s.store, 25).await,
        0,
        "success never replayed"
    );
}

// Initialize and fail one attempt, then change live authority before its NEXT
// claim. No test inspects recipient fingerprints or private Claim fields.
async fn refusal(pool: PgPool, mutation: &str) {
    let s = seed(pool).await;
    fire(&s, s.team, s.key, 169).await;
    // Keep a younger aged hold as well: removing only the critical hold
    // invalidates critical mail without stepping the open incident down.
    hold_since(
        &s,
        s.team,
        s.key,
        expiry_now(&s).await - TimeDelta::hours(25),
    )
    .await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    let (_, claim, _) = outbox::claim(&s.store, Uuid::new_v4(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = claim.expect("initially eligible admin");
    assert!(
        outbox::complete(
            &s.store,
            &claim,
            Err(crate::email::DeliveryError::Connection)
        )
        .await
        .unwrap()
    );
    sqlx::raw_sql(mutation).execute(&s.pool).await.unwrap();
    outbox_due(&s).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(
        rx.try_recv().is_err(),
        "ineligible retry must not reach SMTP"
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible' AND attempt_count=1").await, 1);
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='no_recipients' AND sent=0 AND recipients=1").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_revoked_admin_even_if_still_entitled(pool: PgPool) {
    refusal(pool, "INSERT INTO platform_role_grants(user_id,role,source) SELECT id,'user','manual' FROM users WHERE email='admin@example.test'; UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE role='admin'").await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_lost_entitlement(pool: PgPool) {
    refusal(
        pool,
        "UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE role='admin'",
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_suspended_account(pool: PgPool) {
    refusal(
        pool,
        "UPDATE users SET disabled_at=clock_timestamp() WHERE email='admin@example.test'",
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_cleaned_account(pool: PgPool) {
    refusal(
        pool,
        "UPDATE users SET cleaned_at=clock_timestamp() WHERE email='admin@example.test'",
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_changed_address_without_adding_new_snapshot_recipient(
    pool: PgPool,
) {
    refusal(
        pool,
        "UPDATE users SET email='changed@example.test' WHERE email='admin@example.test'",
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_reconciled_hold_before_tick_retires_incident(pool: PgPool) {
    refusal(pool, "UPDATE governance_reservations SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE state='unknown'").await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_disabled_workspace(pool: PgPool) {
    refusal(
        pool,
        "UPDATE workspaces SET disabled_at=clock_timestamp() WHERE kind='team'",
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_open_incident(pool: PgPool) {
    refusal(pool, "UPDATE alert_events SET resolved_at=clock_timestamp(),resolution='superseded' WHERE kind='aged_unknown_hold'").await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_critical_band_even_while_younger_aged_hold_remains(pool: PgPool) {
    refusal(pool, "UPDATE governance_reservations r SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE state='unknown' AND EXISTS(SELECT FROM monetary_ledger l WHERE l.execution_id=r.execution_id AND l.admitted_at=r.admitted_at AND l.kind='unknown' AND l.created_at<clock_timestamp()-interval '168 hours')").await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_stale_warning_is_not_claimed_after_scope_enters_critical_band(
    pool: PgPool,
) {
    let s = seed(pool).await;
    fire(&s, s.team, s.key, 25).await;
    hold_since(
        &s,
        s.team,
        s.key,
        expiry_now(&s).await - TimeDelta::hours(169),
    )
    .await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='no_recipients' AND recipients=0 AND attempt_count=0").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_admin_membership_removal_does_not_revoke_platform_authority(pool: PgPool) {
    let s = seed(pool).await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) SELECT $1,id,'admin','manual' FROM users WHERE email='admin@example.test'")
        .bind(s.team).execute(&s.pool).await.unwrap();
    fire(&s, s.team, s.key, 25).await;
    sqlx::query(
        "UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE workspace_id=$1",
    )
    .bind(s.team)
    .execute(&s.pool)
    .await
    .unwrap();
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    deliver_pending(&s.store, 25).await;
    let message = rx.try_recv().unwrap();
    assert_eq!(message.rcpt_to.len(), 1);
    assert!(message.rcpt_to[0].contains("admin@example.test"));
    assert!(rx.try_recv().is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_retry_backoff_and_stale_completion_fencing_are_unchanged(pool: PgPool) {
    let s = seed(pool).await;
    fire(&s, s.team, s.key, 25).await;
    relay(&s, 25).await; // claims/completions only; no network I/O
    let (_, first, _) = outbox::claim(&s.store, Uuid::new_v4(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    let first = first.unwrap();
    assert!(
        outbox::complete(
            &s.store,
            &first,
            Err(crate::email::DeliveryError::Connection)
        )
        .await
        .unwrap()
    );
    let delay: f64 = sqlx::query_scalar("SELECT extract(epoch FROM next_attempt_at-clock_timestamp())::float8 FROM alert_recipient_outbox")
        .fetch_one(&s.pool).await.unwrap();
    assert!(delay > 55.0 && delay <= 60.0);
    assert!(
        outbox::claim(&s.store, Uuid::new_v4(), &Default::default())
            .await
            .unwrap()
            .is_none()
    );
    outbox_due(&s).await;
    let (_, stale, _) = outbox::claim(&s.store, Uuid::new_v4(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    let stale = stale.unwrap();
    sqlx::query("UPDATE alert_recipient_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE state='leased'").execute(&s.pool).await.unwrap();
    let (_, next, _) = outbox::claim(&s.store, Uuid::new_v4(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(!outbox::complete(&s.store, &stale, Ok(())).await.unwrap());
    assert!(
        outbox::complete(&s.store, &next.unwrap(), Ok(()))
            .await
            .unwrap()
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='sent' AND sent=1 AND attempt_count=3").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_recipient_snapshot_is_capped_at_fifty(pool: PgPool) {
    let s = seed(pool).await;
    for n in 0..55 {
        user(&s, &format!("admin-{n:02}@example.test"), Some("admin")).await;
    }
    fire(&s, s.team, s.key, 25).await;
    relay(&s, 25).await;
    let (_, claim, _) = outbox::claim(&s.store, Uuid::new_v4(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(claim.is_some());
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        50
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE recipients=50 AND quota_charged"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_rechecks_admin_between_smtp_handoffs(pool: PgPool) {
    let s = seed(pool).await;
    user(&s, "second-admin@example.test", Some("admin")).await;
    fire(&s, s.team, s.key, 25).await;
    let (port, mut rx, gate) = mock::serve_gated().await;
    relay(&s, port).await;
    let store = s.store.clone();
    let sending = tokio::spawn(async move { deliver_pending(&store, 25).await });
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let revoke = if first
        .rcpt_to
        .join(",")
        .contains("second-admin@example.test")
    {
        "admin@example.test"
    } else {
        assert!(first.rcpt_to.join(",").contains("admin@example.test"));
        "second-admin@example.test"
    };
    sqlx::query("UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE user_id=(SELECT id FROM users WHERE email=$1)")
        .bind(revoke).execute(&s.pool).await.unwrap();
    user(&s, "new-admin@example.test", Some("admin")).await;
    gate.add_permits(2);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), sending)
            .await
            .unwrap()
            .unwrap(),
        1
    );
    assert!(
        rx.try_recv().is_err(),
        "revoked and newly added admins must not receive a later handoff"
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='sent' AND recipients=2 AND sent=1 AND attempt_count=1").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_email_quota_suppressed_incidents_never_snapshot_or_claim(pool: PgPool) {
    let s = seed(pool).await;
    let events = generation_events(&s, Some(s.team), 100).await;
    sqlx::query(GENERATE)
        .bind(events)
        .execute(&s.pool)
        .await
        .unwrap();
    // Dispose of unrelated pending fixtures without releasing their quota.
    sqlx::query("UPDATE alert_deliveries SET status='not_configured',completed_at=clock_timestamp() WHERE status='pending'").execute(&s.pool).await.unwrap();
    fire(&s, s.team, s.key, 169).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        0
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='suppressed' AND suppression_reason='workspace_generation_limit' AND NOT quota_charged AND recipients=0 AND attempt_count=0").await, 1);
}
