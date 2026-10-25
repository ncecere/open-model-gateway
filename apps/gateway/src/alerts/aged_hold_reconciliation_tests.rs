//! #101 D review: real reconciliation versus first and subsequent claims.
use super::email::assert_aged_email;
use super::*;

async fn priced_hold(s: &Seed, hours: i32) -> Uuid {
    let (execution, price) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,50,1)")
        .bind(price).bind(s.deployment).execute(&s.pool).await.unwrap();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,completed_at) VALUES($1,$2,$3,$4,'alerts-model','openai_compatible',false,'failed',$1,clock_timestamp()-make_interval(hours=>$5),clock_timestamp()-make_interval(hours=>$5))")
        .bind(execution).bind(s.team).bind(s.key).bind(s.deployment).bind(hours).execute(&s.pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost) SELECT id,workspace_id,api_key_id,deployment_id,$2,started_at,date_trunc('minute',started_at),date_trunc('month',started_at),started_at,'unknown',1,false FROM inference_executions WHERE id=$1")
        .bind(execution).bind(price).execute(&s.pool).await.unwrap();
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,created_at) SELECT gen_random_uuid(),execution_id,admitted_at,'unknown',admitted_at FROM governance_reservations WHERE execution_id=$1")
        .bind(execution).execute(&s.pool).await.unwrap();
    execution
}

async fn reconcile_hold(s: &Seed, execution: Uuid) {
    let admin: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='admin@example.test'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    crate::governance::resolve_usage(
        &s.store,
        s.team,
        execution,
        crate::inference::types::Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            ..Default::default()
        },
        "aged-hold-review-reconciliation",
        admin,
    )
    .await
    .unwrap();
    let settled: bool = sqlx::query_scalar("SELECT state='settled' AND actual_microusd=2 FROM governance_reservations WHERE execution_id=$1")
        .bind(execution).fetch_one(&s.pool).await.unwrap();
    assert!(settled);
    let evidence: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation' AND evidence='aged-hold-review-reconciliation'")
        .bind(execution).fetch_one(&s.pool).await.unwrap();
    assert_eq!(evidence, 1);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE resource_id=$1 AND action='usage.reconciled'",
    )
    .bind(execution)
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_real_reconciliation_before_first_claim_suppresses_mail_and_retires_silently(
    pool: PgPool,
) {
    let s = seed(pool).await;
    let execution = priced_hold(&s, 25).await;
    assert_eq!(tick(&s, expiry_now(&s).await).await.fired, 1);
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    reconcile_hold(&s, execution).await;
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolved_at IS NULL"
        )
        .await,
        1,
        "live claim must refuse before the next retirement tick"
    );
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='no_recipients' AND attempt_count=0 AND recipients=0").await, 1);
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        0
    );
    assert_eq!(tick(&s, expiry_now(&s).await).await.resolved, 1);
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        0
    );
    assert!(rx.try_recv().is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_real_reconciliation_between_eligibility_read_and_first_lease_keeps_authorized_claim(
    pool: PgPool,
) {
    let s = seed(pool).await;
    let execution = priced_hold(&s, 25).await;
    assert_eq!(tick(&s, expiry_now(&s).await).await.fired, 1);
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    // Force the documented READ COMMITTED handoff race: eligibility was
    // already read, then reconciliation commits, then the claim persists.
    let (_, claim, _) = tokio::time::timeout(
        Duration::from_secs(5),
        outbox::claim_with_hook(
            &s.store,
            Uuid::new_v4(),
            &Default::default(),
            reconcile_hold(&s, execution),
        ),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let claim = claim.expect("already authorized claim may finish");
    assert!(
        rx.try_recv().is_err(),
        "claim transaction never performs SMTP"
    );
    assert!(
        outbox::complete(
            &s.store,
            &claim,
            Err(crate::email::DeliveryError::Connection)
        )
        .await
        .unwrap()
    );
    outbox_due(&s).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(
        rx.try_recv().is_err(),
        "next claim must see committed reconciliation"
    );
    assert_eq!(count(&s, "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible' AND attempt_count=1").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_real_reconciliation_during_smtp_accepts_claimed_mail_but_refuses_next_recipient(
    pool: PgPool,
) {
    let s = seed(pool).await;
    sqlx::raw_sql("WITH u AS (INSERT INTO users(id,email) VALUES(gen_random_uuid(),'second-admin@example.test') RETURNING id) INSERT INTO platform_role_grants(user_id,role,source) SELECT id,'admin','manual' FROM u")
        .execute(&s.pool).await.unwrap();
    let execution = priced_hold(&s, 25).await;
    assert_eq!(tick(&s, expiry_now(&s).await).await.fired, 1);
    let (port, mut rx, gate) = mock::serve_gated().await;
    relay(&s, port).await;
    let store = s.store.clone();
    let sending = tokio::spawn(async move { deliver_pending(&store, 25).await });
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_aged_email(&s, &first.data, 24, false).await;
    tokio::time::timeout(Duration::from_secs(5), reconcile_hold(&s, execution))
        .await
        .unwrap();
    // The first send has already been claimed; the second recipient has not.
    gate.add_permits(2);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), sending)
            .await
            .unwrap()
            .unwrap(),
        1
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='sent' AND recipients=2 AND sent=1 AND attempt_count=1").await, 1);
    assert_eq!(count(&s, "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible' AND attempt_count=0").await, 1);
    assert_eq!(tick(&s, expiry_now(&s).await).await.resolved, 1);
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        0
    );
    assert!(rx.try_recv().is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_warning_then_critical_smtp_and_real_reconciliation_retirement(pool: PgPool) {
    let s = seed(pool).await;
    let warning_hold = priced_hold(&s, 50).await;
    // Captured DATABASE time, backdated only for this evaluator fixture, lets
    // the cooldown pass without sleeping or changing immutable incident rows.
    let warning_at = expiry_now(&s).await - TimeDelta::hours(25);
    assert_eq!(tick(&s, warning_at).await.fired, 1);
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert_aged_email(&s, &rx.try_recv().unwrap().data, 24, false).await;
    assert!(rx.try_recv().is_err());
    let critical_hold = priced_hold(&s, 169).await;
    assert_eq!(
        tick(&s, expiry_now(&s).await).await,
        Changes {
            fired: 1,
            resolved: 1
        }
    );
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert_aged_email(&s, &rx.try_recv().unwrap().data, 168, true).await;
    assert!(
        rx.try_recv().is_err(),
        "superseding must not email a resolution"
    );
    reconcile_hold(&s, warning_hold).await;
    reconcile_hold(&s, critical_hold).await;
    assert_eq!(
        tick(&s, expiry_now(&s).await).await,
        Changes {
            fired: 0,
            resolved: 1
        }
    );
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    assert!(rx.try_recv().is_err(), "clearing is silent");
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='superseded'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='cleared'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='fired' AND status='sent'"
        )
        .await,
        2
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
