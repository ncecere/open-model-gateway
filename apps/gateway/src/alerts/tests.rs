//! Evaluator against a real database: thresholds, idempotency, resolution,
//! unknown cost, spikes, error rates, failing connections, built-in personal
//! alerts, replica exclusion and email delivery through a local mock relay.
use super::*;
use crate::email::mock::{self, Behaviour};
use sqlx::PgPool;

async fn expiry_now(s: &Seed) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&s.pool)
        .await
        .unwrap()
}
async fn expiry_tick(s: &Seed, now: DateTime<Utc>) -> Changes {
    let mut tx = s.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(EVALUATION_LOCK)
        .execute(&mut *tx)
        .await
        .unwrap();
    let changes = key_expiring_tick(&mut tx, now).await.unwrap();
    tx.commit().await.unwrap();
    changes
}
async fn expiry_key(s: &Seed, expires: Option<DateTime<Utc>>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,expires_at) VALUES($1,$2,$3,'Never serialize this key name',decode(repeat('12',32),'hex'),$4)")
        .bind(id).bind(s.team).bind(s.owner).bind(expires).execute(&s.pool).await.unwrap();
    id
}
async fn set_expiry(s: &Seed, key: Uuid, expires: Option<DateTime<Utc>>) {
    sqlx::query("UPDATE api_keys SET expires_at=$2 WHERE id=$1")
        .bind(key)
        .bind(expires)
        .execute(&s.pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_exact_bands_and_first_urgent_only(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    let day = chrono::Duration::days(1);
    let us = chrono::Duration::microseconds(1);
    let mut expected = Vec::new();
    for (delta, threshold) in [
        (day * 7 + us, None),
        (day * 7, Some(7)),
        (day + us, Some(7)),
        (day, Some(1)),
        (us, Some(1)),
        (chrono::Duration::zero(), None),
        (-us, None),
    ] {
        let k = expiry_key(&s, Some(now + delta)).await;
        if let Some(t) = threshold {
            expected.push((k, t));
        }
    }
    expiry_key(&s, None).await;
    assert_eq!(expiry_tick(&s, now).await.fired, 4);
    let mut got: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT api_key_id,threshold_days FROM alert_events WHERE kind='key_expiring'",
    )
    .fetch_all(&s.pool)
    .await
    .unwrap();
    expected.sort();
    got.sort();
    assert_eq!(got, expected);
    assert_eq!(expiry_tick(&s, now).await, Changes::default());
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='fired'"
        )
        .await,
        4
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_cycles_rotation_expiry_changes_and_silent_retirement(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    let expiry = now + chrono::Duration::days(3);
    set_expiry(&s, s.key, Some(expiry)).await;
    assert_eq!(expiry_tick(&s, now).await.fired, 1);
    assert_eq!(
        expiry_tick(&s, now + chrono::Duration::days(2)).await,
        Changes {
            fired: 1,
            resolved: 1
        }
    );
    assert_eq!(
        expiry_tick(&s, expiry).await.resolved,
        1,
        "expired urgent incident clears"
    );
    // Extending an expiry retires the old cycle even if it is still in the warning band.
    set_expiry(&s, s.key, Some(expiry + chrono::Duration::days(1))).await;
    assert_eq!(expiry_tick(&s, now).await.fired, 1);
    set_expiry(&s, s.key, Some(expiry + chrono::Duration::days(2))).await;
    assert_eq!(
        expiry_tick(&s, now).await,
        Changes {
            fired: 1,
            resolved: 1
        }
    );
    set_expiry(&s, s.key, Some(expiry)).await;
    assert_eq!(
        expiry_tick(&s, now).await,
        Changes {
            fired: 0,
            resolved: 1
        },
        "restoring an old tuple never replays"
    );
    let resolutions: Vec<(DateTime<Utc>, i32, String)> = sqlx::query_as("SELECT key_expires_at,threshold_days,resolution FROM alert_events WHERE api_key_id=$1 ORDER BY key_expires_at,threshold_days")
        .bind(s.key).fetch_all(&s.pool).await.unwrap();
    assert_eq!(
        resolutions,
        vec![
            (expiry, 1, "cleared".to_owned()),
            (expiry, 7, "superseded".to_owned()),
            (expiry + chrono::Duration::days(1), 7, "cleared".to_owned()),
            (expiry + chrono::Duration::days(2), 7, "cleared".to_owned()),
        ]
    );
    // Real credential rotation: same lineage, different credential id.
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(s.key)
        .execute(&s.pool)
        .await
        .unwrap();
    let rotated = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id,expires_at) VALUES($1,$2,$3,'rotated',decode(repeat('22',32),'hex'),$4,$5)")
        .bind(rotated).bind(s.team).bind(s.owner).bind(s.key).bind(expiry).execute(&s.pool).await.unwrap();
    assert_eq!(expiry_tick(&s, now).await.fired, 1);
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
async fn key_expiry_workspace_rolling_cap_defers_without_starving_other_workspaces(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    // More than a global tick's worth, earlier than the quiet workspace's key.
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,expires_at) SELECT gen_random_uuid(),$1,$2,'backlog',decode(repeat('23',32),'hex'),$3 FROM generate_series(1,601)")
        .bind(s.team).bind(s.owner).bind(now+chrono::Duration::days(3)).execute(&s.pool).await.unwrap();
    set_expiry(&s, s.personal_key, Some(now + chrono::Duration::days(4))).await;
    assert_eq!(
        expiry_tick(&s, now).await.fired,
        101,
        "100 noisy + 1 quiet in the same tick"
    );
    assert_eq!(expiry_tick(&s, expiry_now(&s).await).await.fired, 0);
    let counts: Vec<(Uuid, i64)> = sqlx::query_as("SELECT workspace_id,count(*) FROM alert_events WHERE kind='key_expiring' GROUP BY workspace_id ORDER BY workspace_id")
        .fetch_all(&s.pool).await.unwrap();
    let mut expected = vec![(s.team, 100), (s.personal, 1)];
    expected.sort();
    assert_eq!(counts, expected);
    let later = now + chrono::Duration::hours(24);
    assert_eq!(
        expiry_tick(&s, later - chrono::Duration::microseconds(1))
            .await
            .fired,
        0,
        "still capped just inside the rolling window"
    );
    assert_eq!(
        expiry_tick(&s, later).await.fired,
        100,
        "deferred tuples were not permanently deduplicated"
    );
    assert_eq!(
        expiry_tick(&s, later).await.fired,
        0,
        "new events count toward the next window"
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE kind='key_expiring'"
        )
        .await,
        201
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_bounded_progress_does_not_starve_retirements(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    // 1001 keys across eleven workspaces, each below the rolling cap.
    for _ in 0..11 {
        let ws = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Backlog','team')")
            .bind(ws)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'owner','manual')")
            .bind(ws).bind(s.owner).execute(&s.pool).await.unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,expires_at) SELECT gen_random_uuid(),$1,$2,'backlog',decode(repeat('23',32),'hex'),$3 FROM generate_series(1,91)")
            .bind(ws).bind(s.owner).bind(now+chrono::Duration::days(4)).execute(&s.pool).await.unwrap();
    }
    for n in [500, 500, 1, 0] {
        assert_eq!(expiry_tick(&s, now).await.fired, n);
    }
    // Keep the oldest 500 eligible. Obsolete rows beyond them must not starve.
    sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id IN (SELECT api_key_id FROM alert_events ORDER BY fired_at,id OFFSET 500)").execute(&s.pool).await.unwrap();
    for n in [500, 1, 0] {
        assert_eq!(expiry_tick(&s, now).await.resolved, n);
    }
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolved_at IS NULL"
        )
        .await,
        500
    );
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_deliveries").await,
        1001
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_exclusions_and_reenable_without_replay(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    let expiry = now + chrono::Duration::days(3);
    let revoked = expiry_key(&s, Some(expiry)).await;
    let disabled = expiry_key(&s, Some(expiry)).await;
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(revoked)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id=$1")
        .bind(disabled)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(expiry_tick(&s, now).await.fired, 0);
    sqlx::query("UPDATE api_keys SET disabled_at=NULL WHERE id=$1")
        .bind(disabled)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(expiry_tick(&s, now).await.fired, 1);
    sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id=$1")
        .bind(disabled)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(expiry_tick(&s, now).await.resolved, 1);
    sqlx::query("UPDATE api_keys SET disabled_at=NULL WHERE id=$1")
        .bind(disabled)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(expiry_tick(&s, now).await.fired, 0);
    expiry_key(&s, Some(expiry)).await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1")
        .bind(s.team)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        expiry_tick(&s, now).await.fired,
        0,
        "holder membership lost"
    );
    set_expiry(&s, s.personal_key, Some(expiry)).await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(s.owner)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(expiry_tick(&s, now).await.fired, 0, "disabled holder");
    sqlx::query("UPDATE users SET disabled_at=NULL WHERE id=$1")
        .bind(s.owner)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(s.personal)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(expiry_tick(&s, now).await.fired, 0, "disabled workspace");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_concurrent_evaluation_restart_and_fencing(pool: PgPool) {
    let s = seed(pool).await;
    set_expiry(
        &s,
        s.key,
        Some(expiry_now(&s).await + chrono::Duration::hours(12)),
    )
    .await;
    let stale = crate::leases::Fence {
        lease: crate::leases::Lease::Alerts,
        holder: Uuid::new_v4(),
        epoch: 99,
    };
    let err = evaluate_once_fenced(&s.store, Some(&stale))
        .await
        .unwrap_err();
    assert!(crate::leases::is_fenced(&err));
    assert_eq!(count(&s, "SELECT count(*) FROM alert_events").await, 0);
    let (a, b) = tokio::join!(evaluate_once(&s.store), evaluate_once(&s.store));
    assert_eq!(
        a.unwrap().map_or(0, |r| r.fired) + b.unwrap().map_or(0, |r| r.fired),
        1
    );
    let restarted = Store::new(s.pool.clone());
    assert_eq!(evaluate_once(&restarted).await.unwrap().unwrap().fired, 0);
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries").await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_live_delivery_suppresses_obsolete_and_hides_secrets(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    set_expiry(&s, s.personal_key, Some(now + chrono::Duration::hours(12))).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let mail = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(mail.rcpt_to.join(",").contains("owner@example.test"));
    assert!(mail.data.contains("An API key expires within 1 day"));
    assert!(mail.data.contains("Notifications"));
    assert!(mail.data.contains("Expires:"));
    for (index, forbidden) in [
        "Private key name",
        "Secret key name",
        &s.personal_key.to_string(),
        &s.owner.to_string(),
        "0101010101010101",
        "owner@example.test",
    ]
    .iter()
    .enumerate()
    {
        // The address is necessarily in the SMTP envelope/To header, but never in the body.
        let body = mail.data.split_once("\r\n\r\n").unwrap().1;
        // Report which forbidden value leaked by position, never the value itself.
        assert!(
            !body.contains(forbidden),
            "forbidden value #{index} found in the email body"
        );
    }
    // A queued reminder becomes obsolete before send: never email it, even before the next evaluation.
    let k = expiry_key(&s, Some(now + chrono::Duration::days(3))).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(k)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='no_recipients'"
        )
        .await,
        1
    );
    assert!(rx.try_recv().is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_send_time_loss_and_terminal_outcomes(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    // Every transition is made after queueing, before delivery, without an intervening tick.
    for loss in [
        "revoked",
        "disabled",
        "expired",
        "no_expiry",
        "changed_expiry",
        "membership",
        "platform",
        "user",
        "cleaned",
        "workspace",
    ] {
        let now = expiry_now(&s).await;
        let k = expiry_key(&s, Some(now + chrono::Duration::days(3))).await;
        assert_eq!(evaluate(&s).await.fired, 1, "{loss}");
        let sql = match loss {
            "revoked" => "UPDATE api_keys SET revoked_at=now() WHERE id=$1",
            "disabled" => "UPDATE api_keys SET disabled_at=now() WHERE id=$1",
            "expired" => "UPDATE api_keys SET expires_at=now() WHERE id=$1",
            "no_expiry" => "UPDATE api_keys SET expires_at=NULL WHERE id=$1",
            "changed_expiry" => {
                "UPDATE api_keys SET expires_at=now()+interval '4 days' WHERE id=$1"
            }
            "membership" => {
                "UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1"
            }
            "platform" => "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1",
            "user" => "UPDATE users SET disabled_at=now() WHERE id=$1",
            "cleaned" => "UPDATE users SET cleaned_at=now() WHERE id=$1",
            "workspace" => "UPDATE workspaces SET disabled_at=now() WHERE id=$1",
            _ => unreachable!(),
        };
        let id = match loss {
            "membership" | "platform" | "user" | "cleaned" => s.owner,
            "workspace" => s.team,
            _ => k,
        };
        sqlx::query(sql).bind(id).execute(&s.pool).await.unwrap();
        assert_eq!(deliver_pending(&s.store, 10).await, 1, "{loss}");
        let status: String = sqlx::query_scalar("SELECT d.status FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.api_key_id=$1").bind(k).fetch_one(&s.pool).await.unwrap();
        assert_eq!(status, "no_recipients", "{loss}");
        assert!(rx.try_recv().is_err(), "{loss}");
        // Restore authority (test setup), then keep this key permanently ineligible.
        sqlx::query("UPDATE users SET disabled_at=NULL,cleaned_at=NULL WHERE id=$1")
            .bind(s.owner)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET disabled_at=NULL WHERE id=$1")
            .bind(s.team)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) SELECT DISTINCT workspace_id,user_id,role,'manual' FROM workspace_membership_grants WHERE user_id=$1 ON CONFLICT DO NOTHING")
            .bind(s.owner)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual') ON CONFLICT DO NOTHING")
            .bind(s.owner)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE id=$1")
            .bind(k)
            .execute(&s.pool)
            .await
            .unwrap();
    }
    // Superseded/cleared incidents and changed-band reminders also suppress old pending mail.
    let now = expiry_now(&s).await;
    let k = expiry_key(&s, Some(now + chrono::Duration::days(3))).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    sqlx::query(
        "UPDATE alert_events SET resolved_at=now(),resolution='superseded' WHERE api_key_id=$1",
    )
    .bind(k)
    .execute(&s.pool)
    .await
    .unwrap();
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    assert!(rx.try_recv().is_err());
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(k)
        .execute(&s.pool)
        .await
        .unwrap();
    let (port, _rx) = mock::serve(Behaviour {
        reject_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    expiry_key(&s, Some(now + chrono::Duration::hours(12))).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let status: (String, Option<String>) =
        sqlx::query_as("SELECT status,error FROM alert_deliveries WHERE status='failed'")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(status, ("failed".to_owned(), Some("rejected".to_owned())));
    assert_eq!(
        deliver_pending(&s.store, 10).await,
        0,
        "terminal failure is not retried"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_service_recipients_are_current_owners_admins_not_platform_roles(pool: PgPool) {
    let s = seed(pool).await;
    let sa = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'automation')")
        .bind(sa)
        .bind(s.team)
        .execute(&s.pool)
        .await
        .unwrap();
    let mut users = Vec::new();
    for (email, role) in [
        ("delegate@example.test", "admin"),
        ("member@example.test", "member"),
    ] {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(email)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')",
        )
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')").bind(s.team).bind(id).bind(role).execute(&s.pool).await.unwrap();
        users.push(id);
    }
    let k = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash,expires_at) VALUES($1,$2,$3,'Private automation name',decode(repeat('24',32),'hex'),now()+interval '3 days')").bind(k).bind(s.team).bind(sa).execute(&s.pool).await.unwrap();
    sqlx::query("UPDATE service_accounts SET disabled_at=now() WHERE id=$1")
        .bind(sa)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.fired, 0);
    sqlx::query("UPDATE service_accounts SET disabled_at=NULL WHERE id=$1")
        .bind(sa)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.fired, 1);
    // A former administrator is not entitled merely because they created a service account/key.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(users[0])
        .execute(&s.pool)
        .await
        .unwrap();
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let mail = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(mail.rcpt_to.join(",").contains("owner@example.test"));
    assert!(!mail.data.contains("Private automation name"));
    assert!(rx.try_recv().is_err());
    // A distinct expiry is a new cycle. Current owners and admins receive it, never members/platform-only admins.
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'admin','manual')")
        .bind(s.team)
        .bind(users[0])
        .execute(&s.pool)
        .await
        .unwrap();
    set_expiry(
        &s,
        k,
        Some(expiry_now(&s).await + chrono::Duration::days(4)),
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let mut recipients = Vec::new();
    for _ in 0..2 {
        recipients.extend(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
                .rcpt_to,
        );
    }
    let recipients = recipients.join(",");
    assert!(
        recipients.contains("owner@example.test") && recipients.contains("delegate@example.test"),
        "{recipients}"
    );
    assert!(
        !recipients.contains("member@example.test") && !recipients.contains("admin@example.test")
    );
    assert!(rx.try_recv().is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_schema_rejects_ambiguous_shapes_and_mutable_attribution(pool: PgPool) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    set_expiry(&s, s.key, Some(now + chrono::Duration::days(3))).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    for (column, value) in [
        ("api_key_id", "NULL"),
        ("key_expires_at", "now()"),
        ("threshold_days", "1"),
        ("kind", "'budget_threshold'"),
        ("workspace_id", "NULL"),
        ("severity", "'critical'"),
    ] {
        let result=sqlx::query(&format!("UPDATE alert_events SET {column}={value},resolved_at=now(),resolution='cleared' WHERE kind='key_expiring'")).execute(&s.pool).await;
        assert!(result.is_err(), "mutable {column}");
    }
    let r = rule(
        &s,
        "scope,kind,budget_layers,thresholds",
        "'installation','budget_threshold',ARRAY['local'],ARRAY[80]",
    )
    .await;
    // Both source columns must be NULL for key-expiry; no partial attribution on other kinds.
    for sql in [
        format!(
            "INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,api_key_id,key_expires_at,threshold_days) VALUES(gen_random_uuid(),'{r}','key_expiring','other',7,'warning','{}','x','{}',now(),7)",
            s.team, s.key
        ),
        format!(
            "INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary,api_key_id) VALUES(gen_random_uuid(),'{r}','budget_threshold','other',80,'warning','x','{}')",
            s.key
        ),
    ] {
        assert!(sqlx::query(&sql).execute(&s.pool).await.is_err());
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_job_transaction_itself_holds_fence_and_advisory_lock(pool: PgPool) {
    let s = seed(pool).await;
    set_expiry(
        &s,
        s.key,
        Some(expiry_now(&s).await + chrono::Duration::days(3)),
    )
    .await;
    let leases = crate::leases::Leases::new();
    assert!(leases.renew_once(&s.pool).await > 0);
    let fence = leases.held(crate::leases::Lease::Alerts).unwrap();
    // Inspect locks in the INSERT transaction, not the earlier claim/rule
    // transactions. Removing the key-reminder job's fence or advisory lock
    // makes this trigger abort the insert and the assertion below fail.
    sqlx::raw_sql(&format!("CREATE FUNCTION expiry_assert_guards() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_locks WHERE pid=pg_backend_pid() AND relation='work_leases'::regclass AND mode='RowShareLock' AND granted) OR NOT EXISTS(SELECT 1 FROM pg_locks WHERE pid=pg_backend_pid() AND locktype='advisory' AND classid=0 AND objid={EVALUATION_LOCK} AND objsubid=1 AND granted) THEN RAISE EXCEPTION 'missing expiry job fence or lock'; END IF; RETURN NEW; END $$; CREATE TRIGGER expiry_assert_guards BEFORE INSERT ON alert_events FOR EACH ROW WHEN (NEW.kind='key_expiring') EXECUTE FUNCTION expiry_assert_guards();"))
        .execute(&s.pool).await.unwrap();
    let report = evaluate_once_fenced(&s.store, Some(&fence))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((report.fired, report.failed_rules), (1, 0), "{report:?}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_uses_database_time_in_its_own_job_not_before_rules(pool: PgPool) {
    let s = seed(pool).await;
    set_expiry(
        &s,
        s.key,
        Some(expiry_now(&s).await + chrono::Duration::days(3)),
    )
    .await;
    budget(&s, "local", Some(s.team), "month", 100).await;
    spend(&s, 100).await;
    rule(
        &s,
        "scope,kind,budget_layers,thresholds",
        "'installation','budget_threshold',ARRAY['local'],ARRAY[100]",
    )
    .await;
    // Deterministically cross expiry after claim time, while an earlier rule
    // runs. No clock sleeps: expiry is set to DB time by the earlier rule.
    sqlx::raw_sql(&format!("CREATE FUNCTION expire_during_rule() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE api_keys SET expires_at=clock_timestamp() WHERE id='{}'; RETURN NEW; END $$; CREATE TRIGGER expire_during_rule BEFORE INSERT ON alert_events FOR EACH ROW WHEN (NEW.kind='budget_threshold') EXECUTE FUNCTION expire_during_rule();",s.key)).execute(&s.pool).await.unwrap();
    let report = evaluate(&s).await;
    assert_eq!((report.fired, report.failed_rules), (1, 0), "{report:?}");
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE kind='key_expiring'"
        )
        .await,
        0,
        "expired during an earlier rule, before key evaluation"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_rechecks_authority_between_smtp_recipients(pool: PgPool) {
    let s = seed(pool).await;
    let delegate = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'zzz-delegate@example.test')")
        .bind(delegate)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(delegate)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'admin','manual')").bind(s.team).bind(delegate).execute(&s.pool).await.unwrap();
    set_expiry(
        &s,
        s.key,
        Some(expiry_now(&s).await + chrono::Duration::days(3)),
    )
    .await;
    let (port, mut rx, gate) = mock::serve_gated().await;
    relay(&s, port).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let store = s.store.clone();
    let sending = tokio::spawn(async move { deliver_pending(&store, 10).await });
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    // The outbox claims one recipient fingerprint at a time, ordered by a
    // per-delivery SHA-256 fingerprint, not by address: which of the two
    // recipients is leased first is not deterministic. Revoke whichever one
    // was NOT sent first, so the test exercises the recheck regardless.
    let first_is_owner = first.rcpt_to.join(",").contains("owner@example.test");
    assert!(
        first_is_owner
            || first
                .rcpt_to
                .join(",")
                .contains("zzz-delegate@example.test"),
        "{:?}",
        first.rcpt_to
    );
    let revoke = if first_is_owner { delegate } else { s.owner };
    // The first email is at the relay; the second handoff has NOT started.
    tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
            .bind(revoke)
            .execute(&s.pool),
    )
    .await
    .unwrap()
    .unwrap();
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
        "a removed second recipient must never reach SMTP"
    );
    let outcome: (String, i32, i32) =
        sqlx::query_as("SELECT status,recipients,sent FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    // Both addresses were eligible and snapshotted when the outbox
    // initialized this delivery (recipients=2); the outbox never rewrites
    // that snapshot, it only stops the now-ineligible one short of SMTP, so
    // the single still-eligible recipient is the only one `sent`.
    assert_eq!(outcome, ("sent".to_owned(), 2, 1));
}

// There is no `key_expiry_send_guard` any more (no per-recipient authority
// lock held across an SMTP handoff): the outbox's `recipients()` recheck
// above, run in a short claim transaction strictly before any SMTP I/O,
// gives the same "never send to an already-obsolete reminder" guarantee
// without holding a DB connection or lock during mail I/O. See
// docs/alerts.md's "Recipient outbox" section.

struct Seed {
    store: Store,
    pool: PgPool,
    owner: Uuid,
    team: Uuid,
    personal: Uuid,
    key: Uuid,
    personal_key: Uuid,
    connection: Uuid,
    deployment: Uuid,
}

async fn seed(pool: PgPool) -> Seed {
    let (admin, owner) = (Uuid::new_v4(), Uuid::new_v4());
    let (team, personal) = (Uuid::new_v4(), Uuid::new_v4());
    let (key, personal_key) = (Uuid::new_v4(), Uuid::new_v4());
    let (model, connection, deployment) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for (id, email, role) in [
        (admin, "admin@example.test", "admin"),
        (owner, "owner@example.test", "user"),
    ] {
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')")
            .bind(id)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Platform team','team')")
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Personal','personal',$2)",
    )
    .bind(personal)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$3,'owner','manual'),($2,$3,'owner','manual')").bind(team).bind(personal).bind(owner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$5,'Secret key name',decode(repeat('00',32),'hex')),($3,$4,$5,'Private key name',decode(repeat('01',32),'hex'))").bind(key).bind(team).bind(personal_key).bind(personal).bind(owner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,'alerts-model')")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Local upstream','openai_compatible','none','http://127.0.0.1:19091/v1',true)").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'x',true)").bind(deployment).bind(model).bind(connection).execute(&pool).await.unwrap();
    Seed {
        store: Store::new(pool.clone()),
        pool,
        owner,
        team,
        personal,
        key,
        personal_key,
        connection,
        deployment,
    }
}

/// One attempt with its reservation, `minutes_ago` before now.
#[allow(clippy::too_many_arguments)]
async fn attempt(
    s: &Seed,
    ws: Uuid,
    key: Uuid,
    state: &str,
    error: Option<&str>,
    reservation: &str,
    amount: Option<i64>,
    minutes_ago: i64,
) {
    let id = Uuid::new_v4();
    let at = format!("now()-make_interval(mins=>{minutes_ago})");
    sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,started_at,completed_at) VALUES($1,$2,$3,$4,'alerts-model','openai_compatible',false,$5,$6,$1,{at},CASE WHEN $5='started' THEN NULL ELSE {at} END)"))
        .bind(id).bind(ws).bind(key).bind(s.deployment).bind(state).bind(error).execute(&s.pool).await.unwrap();
    let settled = reservation == "settled";
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,input_tokens,output_tokens) SELECT $1,$2,$3,$4,e.started_at,date_trunc('minute',e.started_at),date_trunc('month',e.started_at),e.started_at,$5,$6,$7,$8,$9,$9 FROM inference_executions e WHERE e.id=$1")
        .bind(id).bind(ws).bind(key).bind(s.deployment).bind(reservation)
        .bind(if settled { amount } else { None })
        .bind(if settled { None } else { amount })
        .bind(amount.is_none())
        .bind(if settled { Some(1_i64) } else { None })
        .execute(&s.pool).await.unwrap();
}
async fn spend(s: &Seed, amount: i64) {
    attempt(
        s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(amount),
        0,
    )
    .await;
}
async fn budget(s: &Seed, layer: &str, ws: Option<Uuid>, period: &str, amount: i64) {
    sqlx::query("DELETE FROM policy_budgets WHERE layer=$1 AND workspace_id IS NOT DISTINCT FROM $2 AND period=$3").bind(layer).bind(ws).bind(period).execute(&s.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES($1,$2,$3,$4)",
    )
    .bind(layer)
    .bind(ws)
    .bind(period)
    .bind(amount)
    .execute(&s.pool)
    .await
    .unwrap();
}
async fn rule(s: &Seed, columns: &str, values: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(&format!(
        "INSERT INTO alert_rules(id,name,{columns}) VALUES($1,'Rule',{values})"
    ))
    .bind(id)
    .execute(&s.pool)
    .await
    .unwrap();
    id
}
/// Open incidents: (subject, level, details).
async fn open(s: &Seed) -> Vec<(String, i32, Value)> {
    sqlx::query_as("SELECT subject_key,level,details FROM alert_events WHERE resolved_at IS NULL ORDER BY subject_key,level").fetch_all(&s.pool).await.unwrap()
}
async fn evaluate(s: &Seed) -> Report {
    evaluate_once(&s.store)
        .await
        .unwrap()
        .expect("evaluation lock")
}
async fn count(s: &Seed, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&s.pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budget_thresholds_escalate_once_and_resolve(pool: PgPool) {
    let s = seed(pool).await;
    budget(&s, "local", Some(s.team), "month", 10_000_000).await;
    let r = rule(
        &s,
        "scope,kind,budget_layers,thresholds",
        "'installation','budget_threshold',ARRAY['local','key'],ARRAY[50,80,100]",
    )
    .await;
    spend(&s, 4_999_999).await;
    assert_eq!(evaluate(&s).await.fired, 0);
    spend(&s, 1).await; // exactly 50%
    assert_eq!(evaluate(&s).await.fired, 1);
    // Idempotent: repeated evaluation never fires twice.
    for _ in 0..3 {
        let again = evaluate(&s).await;
        assert_eq!((again.fired, again.resolved, again.failed_rules), (0, 0, 0));
    }
    let incidents = open(&s).await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].0, format!("local:{}:-:month", s.team));
    assert_eq!(incidents[0].1, 50);
    assert_eq!(incidents[0].2["used_microusd"], "5000000");
    assert_eq!(incidents[0].2["budget_microusd"], "10000000");
    // Escalation supersedes the 50% incident (no "resolved" email for it).
    spend(&s, 3_500_000).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(open(&s).await[0].1, 80);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='superseded'"
        )
        .await,
        1
    );
    // Unknown cost is flagged, never counted as spend (85% stays 80%, not 100%).
    attempt(
        &s,
        s.team,
        s.key,
        "indeterminate",
        None,
        "unknown",
        Some(50_000_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 0);
    // Raising the budget clears the condition: resolved once, with a "resolved" email queued.
    budget(&s, "local", Some(s.team), "month", 100_000_000).await;
    let cleared = evaluate(&s).await;
    assert_eq!((cleared.fired, cleared.resolved), (0, 1));
    assert!(open(&s).await.is_empty());
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='fired'"
        )
        .await,
        2
    );
    // History rows are append-only: resolution happens once.
    let resolved: Uuid =
        sqlx::query_scalar("SELECT id FROM alert_events WHERE resolution='cleared'")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert!(
        sqlx::query("UPDATE alert_events SET resolved_at=now(),resolution='cleared' WHERE id=$1")
            .bind(resolved)
            .execute(&s.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM alert_events WHERE id=$1")
            .bind(resolved)
            .execute(&s.pool)
            .await
            .is_err()
    );
    // Back over budget with unknown cost present: details carry the flag.
    budget(&s, "local", Some(s.team), "month", 10_000_000).await;
    evaluate(&s).await;
    let incidents = open(&s).await;
    assert_eq!(incidents[0].1, 80);
    assert_eq!(incidents[0].2["unknown_cost_requests"], 1);
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_events WHERE rule_id=$1")
            .bind(r)
            .fetch_one(&s.pool)
            .await
            .unwrap()
            >= 3
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budgets_follow_layers_holds_keys_and_never_platform_scan_personal(pool: PgPool) {
    let s = seed(pool).await;
    // Pending holds count like admission; a pending unbounded hold only raises the flag.
    sqlx::query("INSERT INTO policy_budgets(layer,workspace_id,governance_key_id,period,amount_microusd) VALUES('key',$1,$2,'lifetime',1000000)").bind(s.team).bind(s.key).execute(&s.pool).await.unwrap();
    budget(&s, "local", Some(s.personal), "month", 1_000_000).await;
    attempt(
        &s,
        s.team,
        s.key,
        "started",
        None,
        "pending",
        Some(1_000_000),
        0,
    )
    .await;
    attempt(&s, s.team, s.key, "started", None, "pending", None, 0).await;
    attempt(
        &s,
        s.personal,
        s.personal_key,
        "succeeded",
        None,
        "settled",
        Some(9_000_000),
        0,
    )
    .await;
    rule(
        &s,
        "scope,kind,budget_layers,thresholds",
        "'installation','budget_threshold',ARRAY['key','local'],ARRAY[100]",
    )
    .await;
    // Installation spend (0026): a notification against a reference amount.
    rule(
        &s,
        "scope,kind,thresholds,spend_period,spend_amount_microusd",
        "'installation','spend_threshold',ARRAY[100],'day',10000000",
    )
    .await;
    let report = evaluate(&s).await;
    assert_eq!(report.failed_rules, 0);
    let incidents = open(&s).await;
    let subjects: Vec<&str> = incidents.iter().map(|i| i.0.as_str()).collect();
    // Installation spend: $10 of $10 (personal totals count toward the
    // installation); key lineage: $1 of $1.
    assert!(subjects.contains(&"installation:day"), "{subjects:?}");
    assert!(subjects.contains(&format!("key:{}:{}:lifetime", s.team, s.key).as_str()));
    let key = incidents.iter().find(|i| i.0.starts_with("key:")).unwrap();
    assert_eq!(key.2["unknown_cost_requests"], 1);
    // Personal workspaces are never evaluated by platform rules; their owner's built-in alert fires instead.
    let personal: Vec<(Option<Uuid>, Option<String>)> =
        sqlx::query_as("SELECT rule_id,builtin FROM alert_events WHERE workspace_id=$1")
            .bind(s.personal)
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert_eq!(personal, vec![(None, Some(PERSONAL_BUILTIN.to_owned()))]);
    // Summaries carry no key or owner names.
    let summaries: Vec<String> =
        sqlx::query_scalar("SELECT summary||details::text FROM alert_events")
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert!(summaries.iter().all(|t| !t.contains("Secret key name")
        && !t.contains("Private key name")
        && !t.contains("owner@")));
    // Revoked key lineages can no longer spend: their budgets stop alerting.
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(s.key)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn spend_spikes_use_the_trailing_hourly_average(pool: PgPool) {
    let s = seed(pool).await;
    // $16.80 over the previous 7 days: $0.10/hour on average.
    attempt(
        &s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(16_800_000),
        2 * 24 * 60,
    )
    .await;
    // Older than the baseline window: ignored.
    attempt(
        &s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(900_000_000),
        9 * 24 * 60,
    )
    .await;
    rule(
        &s,
        "scope,workspace_id,kind,spike_factor_percent,min_spend_microusd",
        &format!("'workspace','{}','spend_spike',300,100000", s.team),
    )
    .await;
    spend(&s, 299_999).await;
    assert_eq!(evaluate(&s).await.fired, 0);
    // Unknown cost in the last hour is flagged, not counted.
    attempt(
        &s,
        s.team,
        s.key,
        "indeterminate",
        None,
        "unknown",
        Some(5_000_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 0);
    spend(&s, 1).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let incident = &open(&s).await[0];
    assert_eq!(incident.2["current_microusd"], "300000");
    assert_eq!(incident.2["baseline_hourly_microusd"], "100000");
    assert_eq!(incident.2["ratio"], "3.0");
    assert_eq!(incident.2["unknown_cost_requests"], 1);
    let summary: String = sqlx::query_scalar("SELECT summary FROM alert_events")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(summary, "Spend spike: $0.30 in the last hour");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn error_rates_and_failing_connections_resolve_when_traffic_recovers(pool: PgPool) {
    let s = seed(pool).await;
    rule(
        &s,
        "scope,kind,window_minutes,error_rate_percent,min_requests",
        "'installation','error_rate',15,50,4",
    )
    .await;
    rule(
        &s,
        "scope,kind,window_minutes,consecutive_failures,provider_connection_id",
        &format!("'installation','provider_failing',30,3,'{}'", s.connection),
    )
    .await;
    for minutes in [3, 2, 1] {
        attempt(
            &s,
            s.team,
            s.key,
            "failed",
            Some("upstream_unavailable"),
            "settled",
            Some(0),
            minutes,
        )
        .await;
    }
    // A request-specific rejection neither counts as an upstream failure nor resets the run.
    attempt(
        &s,
        s.team,
        s.key,
        "failed",
        Some("upstream_rejected"),
        "settled",
        Some(0),
        0,
    )
    .await;
    // Outside the window: ignored.
    attempt(
        &s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(0),
        120,
    )
    .await;
    let report = evaluate(&s).await;
    // Error rate: 4 failed of 4 finished; connection: 3 upstream failures in a row.
    assert_eq!(report.fired, 2, "{report:?}");
    let connection: (Option<Uuid>, String) = sqlx::query_as(
        "SELECT provider_connection_id,summary FROM alert_events WHERE kind='provider_failing'",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(
        connection,
        (
            Some(s.connection),
            "Connection failing: 3 upstream failures in a row".to_owned()
        )
    );
    // One success ends the run; the error rate stays over 50% (4 of 5).
    attempt(&s, s.team, s.key, "succeeded", None, "settled", Some(0), 0).await;
    let report = evaluate(&s).await;
    assert_eq!((report.fired, report.resolved), (0, 1));
    for _ in 0..4 {
        attempt(&s, s.team, s.key, "succeeded", None, "settled", Some(0), 0).await;
    }
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert!(open(&s).await.is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn disabled_rules_and_workspaces_close_silently_and_stop_firing(pool: PgPool) {
    let s = seed(pool).await;
    budget(&s, "local", Some(s.team), "month", 1_000_000).await;
    spend(&s, 2_000_000).await;
    let r = rule(
        &s,
        "scope,workspace_id,kind,budget_layers,thresholds,notify_workspace_admins",
        &format!(
            "'workspace','{}','budget_threshold',ARRAY['local'],ARRAY[100],true",
            s.team
        ),
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    sqlx::query("UPDATE alert_rules SET enabled=false WHERE id=$1")
        .bind(r)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='rule_disabled'"
        )
        .await,
        1
    );
    // No "resolved" email for a rule someone turned off.
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        0
    );
    assert_eq!(evaluate(&s).await.fired, 0);
    sqlx::query("UPDATE alert_rules SET enabled=true WHERE id=$1")
        .bind(r)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.fired, 1);
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(s.team)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert!(open(&s).await.is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn one_replica_evaluates_at_a_time(pool: PgPool) {
    let s = seed(pool).await;
    let mut holder = s.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(72419507)")
        .execute(&mut *holder)
        .await
        .unwrap();
    assert!(evaluate_once(&s.store).await.unwrap().is_none());
    holder.rollback().await.unwrap();
    assert!(evaluate_once(&s.store).await.unwrap().is_some());
}

async fn relay(s: &Seed, port: u16) {
    sqlx::query("UPDATE installation_settings SET smtp_host='127.0.0.1',smtp_port=$1,smtp_tls='none',smtp_from_address='gateway@example.test' WHERE singleton").bind(i32::from(port)).execute(&s.pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn emails_reach_recipients_and_failures_are_recorded(pool: PgPool) {
    let s = seed(pool).await;
    // Without a relay, the outcome is recorded and nothing crashes.
    budget(&s, "local", Some(s.personal), "month", 1_000_000).await;
    attempt(
        &s,
        s.personal,
        s.personal_key,
        "succeeded",
        None,
        "settled",
        Some(900_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='not_configured'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT recipients::bigint FROM alert_deliveries WHERE status='not_configured'"
        )
        .await,
        1
    );
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    budget(&s, "local", Some(s.team), "month", 1_000_000).await;
    spend(&s, 1_000_000).await;
    rule(&s, "scope,workspace_id,kind,budget_layers,thresholds,notify_workspace_admins,notify_emails", &format!("'workspace','{}','budget_threshold',ARRAY['local'],ARRAY[100],true,ARRAY['finance@example.test','OWNER@example.test']", s.team)).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let mut to = Vec::new();
    for _ in 0..2 {
        let got = rx.recv().await.unwrap();
        assert!(got.data.contains("Workspace monthly budget reached 100%"));
        assert!(got.data.contains("Team Platform team"));
        assert!(!got.data.contains("Secret key name"));
        to.push(got.rcpt_to.join(","));
    }
    to.sort();
    assert!(
        to[0].contains("finance@example.test") && to[1].contains("owner@example.test"),
        "{to:?}"
    );
    let (status, recipients, sent): (String, i32, i32) = sqlx::query_as("SELECT d.status,d.recipients,d.sent FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.rule_id IS NOT NULL").fetch_one(&s.pool).await.unwrap();
    assert_eq!((status.as_str(), recipients, sent), ("sent", 2, 2));
    // The relay refusing recipients is recorded as a failure category.
    let (port, _rx) = mock::serve(Behaviour {
        reject_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    budget(&s, "local", Some(s.team), "month", 100_000_000).await;
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let (status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status,error FROM alert_deliveries WHERE transition='resolved'")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), error.as_deref()),
        ("failed", Some("rejected"))
    );
    assert_eq!(deliver_pending(&s.store, 10).await, 0);
    let _ = s.owner;
}

/// A connection refusal must retain a durable, scheduled recipient attempt.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_connection_failure_waits_for_due_retry(pool: PgPool) {
    let s = seed(pool).await;
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    relay(&s, port).await;
    let mut tx = s.pool.begin().await.unwrap();
    fire_scim_last_admin(&mut tx, "user").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM alert_deliveries")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        status, "pending",
        "transient failure must not become terminal"
    );
    let (attempts, retrying, waiting): (i32, bool, i32) =
        sqlx::query_as("SELECT attempt_count,retrying,waiting FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((attempts, retrying, waiting), (1, true, 1));
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
}

/// A per-tick budget must stop claiming further recipients once
/// `elapsed + SEND_DEADLINE` would exceed it, so a hanging relay can never
/// cancel a send mid-flight and lose its result: the recipient's lease must
/// never even be taken when there is no budget left for one full send.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deliver_pending_respects_an_injected_budget_against_a_hanging_relay(pool: PgPool) {
    let s = seed(pool).await;
    // DATA is never acknowledged: a claimed send on this relay hangs until
    // SEND_DEADLINE. The gate is kept alive but never released.
    let (port, _rx, _gate) = mock::serve_gated().await;
    relay(&s, port).await;
    outbox_incident(&s, "'hanging@example.test'").await;
    outbox_due(&s).await;
    let touched = tokio::time::timeout(
        Duration::from_secs(5),
        outbox::deliver_pending_within(&s.store, 25, Duration::from_secs(1)),
    )
    .await
    .expect("a budget below one SEND_DEADLINE must never block on a send");
    assert_eq!(touched, 0);
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        0,
        "no lease may be taken when there is no budget for one full send"
    );
    let (status, initialized): (String, bool) =
        sqlx::query_as("SELECT status,initialized FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), initialized), ("pending", false));
}

async fn outbox_incident(s: &Seed, emails: &str) -> Uuid {
    let rule = rule(s, "scope,workspace_id,kind,budget_layers,thresholds,notify_workspace_admins,notify_emails", &format!("'workspace','{}','budget_threshold',ARRAY['local'],ARRAY[100],true,ARRAY[{emails}]::text[]",s.team)).await;
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) VALUES($1,$2,'budget_threshold','outbox',100,'warning',$3,'Outbox test')").bind(event).bind(rule).bind(s.team).execute(&s.pool).await.unwrap();
    sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')")
        .bind(Uuid::new_v4())
        .bind(event)
        .execute(&s.pool)
        .await
        .unwrap();
    rule
}
async fn outbox_due(s: &Seed) {
    sqlx::query("UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second' WHERE state='waiting'").execute(&s.pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_partial_success_is_durable_and_never_resent(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        reject_only: Some("finance@example.test"),
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "'finance@example.test'").await;
    let (a, b) = tokio::join!(deliver_pending(&s.store, 25), deliver_pending(&s.store, 25));
    assert!(a + b >= 1);
    assert!(rx.recv().await.unwrap().rcpt_to[0].contains("owner@example.test"));
    assert!(
        rx.try_recv().is_err(),
        "successful recipient sent once across replicas"
    );
    let (sent, attempts, waiting): (i32, i32, i32) =
        sqlx::query_as("SELECT sent,attempt_count,waiting FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((sent, attempts, waiting), (1, 2, 1));
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    let (port, mut retry) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    outbox_due(&s).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(retry.recv().await.unwrap().rcpt_to[0].contains("finance@example.test"));
    assert!(retry.try_recv().is_err());
    let (status, sent, attempts): (String, i32, i32) =
        sqlx::query_as("SELECT status,sent,attempt_count FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), sent, attempts), ("sent", 2, 3));
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    assert!(
        sqlx::query("UPDATE alert_recipient_outbox SET state='waiting' WHERE state='sent'")
            .execute(&s.pool)
            .await
            .is_err(),
        "completed recipients cannot reopen"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_backoff_and_five_attempt_exhaustion_use_database_clock(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "").await;
    for (index, seconds) in [60, 300, 1800, 7200].into_iter().enumerate() {
        assert_eq!(deliver_pending(&s.store, 25).await, 1);
        let (attempts, due, retrying): (i32,f64,bool) = sqlx::query_as("SELECT attempt_count,extract(epoch FROM next_attempt_at-clock_timestamp())::float8,retrying FROM alert_deliveries").fetch_one(&s.pool).await.unwrap();
        assert_eq!(attempts, index as i32 + 1);
        assert!(retrying && due > f64::from(seconds) - 5.0 && due <= f64::from(seconds));
        assert_eq!(deliver_pending(&s.store, 25).await, 0);
        outbox_due(&s).await;
    }
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    let (status, attempts, exhausted, due): (String, i32, i32, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT status,attempt_count,exhausted,next_attempt_at FROM alert_deliveries",
        )
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!((status.as_str(), attempts, exhausted), ("failed", 5, 1));
    assert!(due.is_none());
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_live_lease_excludes_replicas_and_fences_stale_completion(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    outbox_incident(&s, "").await;
    let (_, old) = outbox::claim(&s.store, Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let old = old.unwrap();
    assert!(
        outbox::claim(&s.store, Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    let attempts: i32 = sqlx::query_scalar("SELECT attempt_count FROM alert_recipient_outbox")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1, "attempt committed before any network I/O");
    let retrying: bool = sqlx::query_scalar("SELECT retrying FROM alert_deliveries")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert!(!retrying, "the first live attempt is not a retry yet");
    sqlx::query("UPDATE alert_recipient_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE state='leased'").execute(&s.pool).await.unwrap();
    let (_, new) = outbox::claim(&s.store, Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let new = new.unwrap();
    assert!(!outbox::complete(&s.store, &old, Ok(())).await.unwrap());
    assert!(outbox::complete(&s.store, &new, Ok(())).await.unwrap());
    assert!(
        !outbox::complete(&s.store, &old, Err(crate::email::DeliveryError::Connection))
            .await
            .unwrap()
    );
    assert!(rx.try_recv().is_err(), "claiming alone never sends");
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='sent' AND attempt_count=2"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_revocation_intersects_snapshot_and_never_adds_recipients(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    let rule = outbox_incident(&s, "'finance@example.test'").await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(s.team).bind(s.owner).execute(&s.pool).await.unwrap();
    sqlx::query("UPDATE alert_rules SET notify_emails=ARRAY['finance@example.test','new@example.test'] WHERE id=$1").bind(rule).execute(&s.pool).await.unwrap();
    outbox_due(&s).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(rx.recv().await.unwrap().rcpt_to[0].contains("finance@example.test"));
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        2
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible'"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_explicit_address_cannot_resurrect_disabled_internal_user(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "'OWNER@example.test'").await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(s.owner)
        .execute(&s.pool)
        .await
        .unwrap();
    outbox_due(&s).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='no_recipients'"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_internal_address_eligibility_survives_cleanup_and_address_change(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "'owner@example.test'").await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    // Account cleanup erases the email; a listed old address must not become
    // an apparently external recipient that bypasses disablement.
    sqlx::query("UPDATE users SET email='redacted@example.test',disabled_at=now(),disable_reason='admin_suspension',cleaned_at=now() WHERE id=$1").bind(s.owner).execute(&s.pool).await.unwrap();
    outbox_due(&s).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible'"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_snapshot_race_keeps_internal_identity(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    outbox_incident(&s, "'OWNER@example.test'").await;
    let read = std::sync::Arc::new(tokio::sync::Notify::new());
    let persist = std::sync::Arc::new(tokio::sync::Notify::new());
    let (seen, resume, store) = (read.clone(), persist.clone(), s.store.clone());
    let worker = tokio::spawn(async move {
        outbox::claim_with_hook(&store, Uuid::new_v4(), async move {
            seen.notify_one();
            resume.notified().await;
        })
        .await
        .unwrap()
        .unwrap()
        .1
        .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), read.notified())
        .await
        .unwrap();
    // A second connection commits after eligibility was read, before INSERT.
    let mut other = s.pool.acquire().await.unwrap();
    sqlx::query("UPDATE users SET email='changed@example.test',disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(s.owner).execute(&mut *other).await.unwrap();
    persist.notify_one();
    let claim = worker.await.unwrap();
    let captured: Option<Uuid> =
        sqlx::query_scalar("SELECT internal_user_id FROM alert_recipient_outbox")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        captured,
        Some(s.owner),
        "snapshot must retain its original internal identity"
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
        "old address must not become an external recipient"
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='ineligible'"
        )
        .await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_accepted_data_ignores_hanging_quit_and_never_retries(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour {
        hang_quit: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "").await;
    tokio::time::timeout(Duration::from_secs(2), deliver_pending(&s.store, 25))
        .await
        .expect("DATA acceptance must not wait for QUIT");
    assert!(rx.recv().await.is_some());
    assert_eq!(count(&s, "SELECT count(*) FROM alert_deliveries WHERE status='sent' AND sent=1 AND attempt_count=1 AND NOT retrying AND waiting=0").await, 1);
    outbox_due(&s).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    assert!(rx.try_recv().is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_unsetting_smtp_preserves_a_live_claim_result(pool: PgPool) {
    let s = seed(pool).await;
    relay(&s, 25).await;
    outbox_incident(&s, "'finance@example.test'").await;
    let (_, claim) = outbox::claim(&s.store, Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let claim = claim.unwrap();
    sqlx::query("UPDATE installation_settings SET smtp_host=NULL,smtp_port=NULL,smtp_tls=NULL,smtp_from_address=NULL WHERE singleton").execute(&s.pool).await.unwrap();
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='pending'"
        )
        .await,
        1
    );
    assert!(outbox::complete(&s.store, &claim, Ok(())).await.unwrap());
    assert_eq!(count(&s,"SELECT count(*) FROM alert_deliveries WHERE status='not_configured' AND sent=1 AND waiting=0").await,1);
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_unset_smtp_and_expired_fifth_claim_never_loop(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "").await;
    deliver_pending(&s.store, 25).await;
    sqlx::query("UPDATE installation_settings SET smtp_host=NULL,smtp_port=NULL,smtp_tls=NULL,smtp_from_address=NULL WHERE singleton").execute(&s.pool).await.unwrap();
    outbox_due(&s).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert_eq!(count(&s,"SELECT count(*) FROM alert_deliveries WHERE status='not_configured' AND NOT retrying AND waiting=0").await,1);
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
    relay(&s, port).await;
    outbox_incident(&s, "").await;
    for _ in 0..4 {
        deliver_pending(&s.store, 25).await;
        outbox_due(&s).await;
    }
    let (_, fifth) = outbox::claim(&s.store, Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let fifth = fifth.unwrap();
    sqlx::query("UPDATE alert_recipient_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE state='leased'").execute(&s.pool).await.unwrap();
    deliver_pending(&s.store, 25).await;
    assert!(!outbox::complete(&s.store, &fifth, Ok(())).await.unwrap());
    assert_eq!(count(&s,"SELECT count(*) FROM alert_recipient_outbox WHERE state='exhausted' AND attempt_count=5").await,1);
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_24h_cleanup_preserves_success_and_exhausts_leases(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        temporary_recipient: true,
        reject_only: Some("finance@example.test"),
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    outbox_incident(&s, "'finance@example.test'").await;
    deliver_pending(&s.store, 25).await;
    outbox_due(&s).await;
    let (_, claim) = outbox::claim(&s.store, Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let claim = claim.unwrap();
    sqlx::query("UPDATE alert_deliveries SET created_at=clock_timestamp()-interval '25 hours'")
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_recipient_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE state='leased'").execute(&s.pool).await.unwrap();
    deliver_pending(&s.store, 25).await;
    assert!(!outbox::complete(&s.store, &claim, Ok(())).await.unwrap());
    let (status, sent, exhausted): (String, i32, i32) =
        sqlx::query_as("SELECT status,sent,exhausted FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), sent, exhausted), ("partial", 1, 1));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_network_io_releases_connection_and_restart_reclaims(pool: PgPool) {
    let s = seed(pool).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    relay(&s, listener.local_addr().unwrap().port()).await;
    outbox_incident(&s, "").await;
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*s.pool.connect_options()).clone())
        .await
        .unwrap();
    let worker = Store::new(single.clone());
    let job = tokio::spawn(async move { deliver_pending(&worker, 25).await });
    // No greeting: SMTP remains in network I/O while the sole DB connection
    // must already be available, and its attempt must be durable.
    let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let attempts: i32 = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar("SELECT attempt_count FROM alert_recipient_outbox WHERE state='leased'")
            .fetch_one(&single),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(attempts, 1);
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    drop(socket);
    drop(listener);
    sqlx::query("UPDATE alert_recipient_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE state='leased'").execute(&s.pool).await.unwrap();
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&Store::new(single.clone()), 25).await, 1);
    assert!(rx.recv().await.unwrap().rcpt_to[0].contains("owner@example.test"));
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='sent' AND attempt_count=2"
        )
        .await,
        1
    );
    single.close().await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_personal_delivery_stays_owner_private(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,workspace_id,summary) VALUES($1,'personal_budget','budget_threshold','private',80,'warning',$2,'Personal budget reached 80%')").bind(event).bind(s.personal).execute(&s.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&s.pool)
    .await
    .unwrap();
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    let got = rx.recv().await.unwrap();
    assert!(got.rcpt_to[0].contains("owner@example.test"));
    assert!(got.data.contains("Your personal workspace"));
    assert!(!got.data.contains("admin@example.test"));
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn outbox_tick_caps_recipient_attempts_at_25(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    for i in 0..3 {
        let emails = (0..10)
            .map(|n| format!("'recipient-{i}-{n}@example.test'"))
            .collect::<Vec<_>>()
            .join(",");
        outbox_incident(&s, &emails).await;
    }
    deliver_pending(&s.store, 1000).await;
    let attempts: i64 =
        sqlx::query_scalar("SELECT sum(attempt_count)::bigint FROM alert_deliveries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 25);
    let mut received = 0;
    while rx.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, 25);
    deliver_pending(&s.store, 1000).await;
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='sent'"
        )
        .await,
        33
    );
}

#[sqlx::test(migrations = false)]
async fn outbox_upgrade_never_replays_terminal_history(pool: PgPool) {
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version <= 48)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    previous.run(&pool).await.unwrap();
    let s = seed(pool).await;
    for status in [
        "sent",
        "partial",
        "failed",
        "not_configured",
        "no_recipients",
    ] {
        let rule = outbox_incident(&s, "").await;
        sqlx::query("UPDATE alert_deliveries SET status=$1,completed_at=now() WHERE event_id IN(SELECT id FROM alert_events WHERE rule_id=$2)")
            .bind(status).bind(rule).execute(&s.pool).await.unwrap();
    }
    // Only this genuinely pre-upgrade pending delivery may initialize.
    outbox_incident(&s, "").await;
    crate::store::MIGRATOR.run(&s.pool).await.unwrap();
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 1);
    assert!(rx.recv().await.unwrap().rcpt_to[0].contains("owner@example.test"));
    assert!(rx.try_recv().is_err());
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        1
    );
    assert!(
        sqlx::query("UPDATE alert_deliveries SET status='pending',completed_at=NULL")
            .execute(&s.pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) SELECT id,sha256('test'::bytea) FROM alert_deliveries").execute(&s.pool).await.is_err());
    assert_eq!(deliver_pending(&s.store, 25).await, 0);
}

/// The former budget-alert scans (before 0025), kept as the oracle.
const OLD_WORKSPACE_BUDGETS: &str = r#"WITH ws AS (SELECT id,kind FROM workspaces WHERE disabled_at IS NULL AND CASE $1::text WHEN 'shared' THEN kind IN ('team','project') WHEN 'personal' THEN kind='personal' ELSE id=$2 AND kind IN ('team','project') END),
b AS (
 SELECT ws.id workspace_id,'type'::text layer,NULL::uuid lineage,p.period,p.amount_microusd FROM ws JOIN policy_budgets p ON p.layer='type' AND p.kind=ws.kind WHERE NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides o WHERE o.workspace_id=ws.id)
 UNION ALL SELECT ws.id,'override',NULL,p.period,p.amount_microusd FROM ws JOIN workspace_platform_policy_overrides o ON o.workspace_id=ws.id JOIN policy_budgets p ON p.layer='override' AND p.workspace_id=ws.id
 UNION ALL SELECT ws.id,'local',NULL,p.period,p.amount_microusd FROM ws JOIN policy_budgets p ON p.layer='local' AND p.workspace_id=ws.id
 UNION ALL SELECT ws.id,'key',p.governance_key_id,p.period,p.amount_microusd FROM ws JOIN policy_budgets p ON p.layer='key' AND p.workspace_id=ws.id WHERE EXISTS(SELECT 1 FROM api_keys k WHERE k.workspace_id=ws.id AND k.governance_key_id=p.governance_key_id AND k.revoked_at IS NULL)
),
win AS (SELECT * FROM (VALUES ('day',$4::timestamptz,$5::timestamptz),('week',$6::timestamptz,$7::timestamptz),('month',$8::timestamptz,$9::timestamptz),('lifetime',$10::timestamptz,$11::timestamptz)) v(period,start_at,end_at))
SELECT b.workspace_id,b.layer,b.lineage,b.period,b.amount_microusd,u.used::text,u.unknown
FROM b JOIN win ON win.period=b.period
CROSS JOIN LATERAL (SELECT coalesce(sum(CASE r.state WHEN 'settled' THEN r.actual_microusd WHEN 'pending' THEN r.held_microusd END),0) used,
  count(*) FILTER(WHERE r.state='unknown' OR (r.state='pending' AND (r.unbounded_cost OR r.held_microusd IS NULL))) unknown
  FROM governance_reservations r WHERE r.workspace_id=b.workspace_id AND r.admitted_at>=win.start_at AND r.admitted_at<win.end_at
  AND (b.lineage IS NULL OR r.api_key_id IN(SELECT k.id FROM api_keys k WHERE k.workspace_id=b.workspace_id AND k.governance_key_id=b.lineage))) u
WHERE b.amount_microusd>0 AND b.layer=ANY($3)
ORDER BY b.workspace_id,b.layer,b.lineage,b.period LIMIT 5001"#;

const OLD_INSTALLATION_BUDGET: &str = "SELECT coalesce(sum(CASE state WHEN 'settled' THEN actual_microusd WHEN 'pending' THEN held_microusd END),0)::text,count(*) FILTER(WHERE state='unknown' OR (state='pending' AND (unbounded_cost OR held_microusd IS NULL))) FROM governance_reservations WHERE admitted_at>=$1 AND admitted_at<$2";

/// Budget alerts read maintained totals (0015/0025) and report exactly what
/// the former history scans reported, for every layer, period and state.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budget_alert_totals_equal_the_former_scans(pool: PgPool) {
    let s = seed(pool).await;
    let rotated = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'rotated',decode(repeat('09',32),'hex'),$4)")
        .bind(rotated).bind(s.team).bind(s.owner).bind(s.key).execute(&s.pool).await.unwrap();
    let mut n = 0i64;
    for (ws, key) in [
        (s.team, s.key),
        (s.team, rotated),
        (s.personal, s.personal_key),
    ] {
        for (state, reservation, amount) in [
            ("succeeded", "settled", Some(70)),
            ("started", "pending", Some(40)),
            ("started", "pending", None),
            ("failed", "unknown", Some(25)),
            ("failed", "unknown", None),
        ] {
            for minutes_ago in [0, 90, 60 * 30, 60 * 24 * 9, 60 * 24 * 40, 60 * 24 * 400] {
                n += 1;
                attempt(
                    &s,
                    ws,
                    key,
                    state,
                    None,
                    reservation,
                    amount.map(|a| a + n),
                    minutes_ago,
                )
                .await;
            }
        }
    }
    // An unknown attempt whose hold is unbounded (marked, finite floor).
    sqlx::query("UPDATE governance_reservations SET unbounded_cost=true WHERE execution_id IN (SELECT execution_id FROM governance_reservations WHERE state='unknown' AND held_microusd IS NOT NULL LIMIT 3)").execute(&s.pool).await.unwrap();
    for period in ["day", "week", "month", "lifetime"] {
        budget(&s, "local", Some(s.team), period, 1_000).await;
        budget(&s, "local", Some(s.personal), period, 1_000).await;
        sqlx::query("INSERT INTO policy_budgets(layer,workspace_id,governance_key_id,period,amount_microusd) VALUES('key',$1,$2,$3,1000)").bind(s.team).bind(s.key).bind(period).execute(&s.pool).await.unwrap();
        sqlx::query("INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','team',$1,1000)").bind(period).execute(&s.pool).await.unwrap();
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let w = windows(now);
    let layers: Vec<String> = BUDGET_LAYERS.iter().map(|l| l.to_string()).collect();
    for (mode, one) in [("shared", None), ("personal", None), ("one", Some(s.team))] {
        let new: Vec<BudgetRow> = sqlx::query_as(WORKSPACE_BUDGETS)
            .bind(mode)
            .bind(one)
            .bind(&layers)
            .bind(w[0].0)
            .bind(w[1].0)
            .bind(w[2].0)
            .bind(w[3].0)
            .fetch_all(&s.pool)
            .await
            .unwrap();
        let old: Vec<BudgetRow> = sqlx::query_as(OLD_WORKSPACE_BUDGETS)
            .bind(mode)
            .bind(one)
            .bind(&layers)
            .bind(w[0].0)
            .bind(w[0].1)
            .bind(w[1].0)
            .bind(w[1].1)
            .bind(w[2].0)
            .bind(w[2].1)
            .bind(w[3].0)
            .bind(w[3].1)
            .fetch_all(&s.pool)
            .await
            .unwrap();
        assert_eq!(new, old, "{mode}");
        assert!(!new.is_empty());
    }
    for (index, period) in BudgetPeriod::ALL.iter().enumerate() {
        let (used, _pending, unknown): (String, String, i64) =
            sqlx::query_as(crate::governance::totals::INSTALLATION_SPEND)
                .bind(period.as_str())
                .bind(w[index].0)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        let new = (used, unknown);
        let old: (String, i64) = sqlx::query_as(OLD_INSTALLATION_BUDGET)
            .bind(w[index].0)
            .bind(w[index].1)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(new, old, "{period:?}");
        assert!(old.1 > 0);
    }
    // The metrics gauge reads the same totals: exact pending/unknown counts.
    let gauge: (i64, i64) = sqlx::query_as(crate::governance::totals::RESERVATION_COUNTS)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let scanned: (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE state='pending'),count(*) FILTER(WHERE state='unknown') FROM governance_reservations").fetch_one(&s.pool).await.unwrap();
    assert_eq!(gauge, scanned);
}

/// Installation spend thresholds (0026): exact integer percentages of a
/// reference amount over the installation-wide window (settled plus pending
/// holds, every workspace), escalation and resolution like budget rules, and
/// no effect on admission (no installation budget exists to deny with).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn installation_spend_thresholds_notify_without_limits(pool: PgPool) {
    let s = seed(pool).await;
    let r = rule(
        &s,
        "scope,kind,thresholds,spend_period,spend_amount_microusd,notify_platform_admins",
        "'installation','spend_threshold',ARRAY[50,100],'month',10000000,true",
    )
    .await;
    spend(&s, 4_999_999).await;
    assert_eq!(evaluate(&s).await.fired, 0);
    // A personal workspace's spend counts toward the installation total.
    attempt(
        &s,
        s.personal,
        s.personal_key,
        "succeeded",
        None,
        "settled",
        Some(1),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let incidents = open(&s).await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].0, "installation:month");
    assert_eq!(incidents[0].1, 50);
    assert_eq!(incidents[0].2["used_microusd"], "5000000");
    assert_eq!(incidents[0].2["spend_amount_microusd"], "10000000");
    // Pending holds count; unknown cost is flagged, never counted as spend.
    attempt(
        &s,
        s.team,
        s.key,
        "started",
        None,
        "pending",
        Some(5_000_000),
        0,
    )
    .await;
    attempt(
        &s,
        s.team,
        s.key,
        "indeterminate",
        None,
        "unknown",
        Some(90_000_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let incidents = open(&s).await;
    assert_eq!(incidents[0].1, 100);
    assert_eq!(incidents[0].2["used_microusd"], "10000000");
    assert_eq!(incidents[0].2["pending_microusd"], "5000000");
    assert_eq!(incidents[0].2["unknown_cost_requests"], 1);
    let summary: String =
        sqlx::query_scalar("SELECT summary FROM alert_events WHERE resolved_at IS NULL")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(summary, "Installation monthly spend reached 100%");
    // Idempotent.
    let again = evaluate(&s).await;
    assert_eq!((again.fired, again.resolved), (0, 0));
    // Raising the reference amount clears it once.
    sqlx::query("UPDATE alert_rules SET spend_amount_microusd=100000000 WHERE id=$1")
        .bind(r)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    // Nothing installation-wide limits admission: no budget rows, no totals scope.
    assert_eq!(
        count(&s, "SELECT count(*) FROM policy_budgets WHERE layer NOT IN ('type','override','local','key')").await,
        0
    );
    // Only installation rules may watch installation spend; shapes are enforced.
    for bad in [
        format!("INSERT INTO alert_rules(id,name,scope,workspace_id,kind,thresholds,spend_period,spend_amount_microusd) VALUES(gen_random_uuid(),'x','workspace','{}','spend_threshold',ARRAY[50],'day',1)", s.team),
        "INSERT INTO alert_rules(id,name,scope,kind,thresholds,spend_period) VALUES(gen_random_uuid(),'x','installation','spend_threshold',ARRAY[50],'day')".to_owned(),
        "INSERT INTO alert_rules(id,name,scope,kind,thresholds,spend_period,spend_amount_microusd,budget_layers) VALUES(gen_random_uuid(),'x','installation','spend_threshold',ARRAY[50],'day',1,ARRAY['local'])".to_owned(),
        "INSERT INTO alert_rules(id,name,scope,kind,budget_layers,thresholds) VALUES(gen_random_uuid(),'x','installation','budget_threshold',ARRAY['installation'],ARRAY[50])".to_owned(),
    ] {
        assert!(sqlx::query(&bad).execute(&s.pool).await.is_err(), "{bad}");
    }
}

/// (id, kind, name, layers, thresholds, spend period, spend amount, live, emails).
type RuleRow = (
    Uuid,
    String,
    String,
    Option<Vec<String>>,
    Option<Vec<i32>>,
    Option<String>,
    Option<i64>,
    bool,
    Vec<String>,
);
/// Upgrading a database that had an installation budget and installation
/// budget alert rules (0025 -> 0026): thresholds and recipients are kept on
/// installation spend rules, open incidents continue or are superseded
/// without a "resolved" email, everything is audited, and history rows
/// (reservations, ledger, executions) are untouched.
#[sqlx::test(migrations = false)]
async fn migration_converts_installation_budget_alerts(pool: PgPool) {
    use sqlx::migrate::Migrator;
    let prefix = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version < 26)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    prefix.run(&pool).await.unwrap();
    let (only, mixed, deleted) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::raw_sql(&format!(r#"
     INSERT INTO installation_policy(singleton,requests_per_minute,concurrent_jobs) VALUES(true,600,4);
     INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',10000000),('installation','day',500000),('installation','week',0);
     INSERT INTO alert_rules(id,scope,kind,name,budget_layers,thresholds,notify_platform_admins,notify_emails) VALUES
      ('{only}','installation','budget_threshold','Installation budget',ARRAY['installation'],ARRAY[50,80,100],true,ARRAY['ops@example.test']),
      ('{mixed}','installation','budget_threshold','Budgets',ARRAY['installation','local','key'],ARRAY[80],false,'{{}}'),
      ('{deleted}','installation','budget_threshold','Old',ARRAY['installation'],ARRAY[80],false,'{{}}');
     UPDATE alert_rules SET deleted_at=now() WHERE id='{deleted}';
     INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary) VALUES
      (gen_random_uuid(),'{only}','budget_threshold','installation:day',80,'warning','Installation daily budget reached 80%'),
      (gen_random_uuid(),'{only}','budget_threshold','installation:month',50,'warning','Installation monthly budget reached 50%'),
      (gen_random_uuid(),'{mixed}','budget_threshold','installation:month',80,'warning','Installation monthly budget reached 80%');
    "#)).execute(&pool).await.unwrap();
    // The upgrade, then a second installation's re-run of the conversion
    // statements would find nothing left to convert.
    crate::store::MIGRATOR.run(&pool).await.unwrap();
    let rules: Vec<RuleRow> = sqlx::query_as(
        "SELECT id,kind,name,budget_layers,thresholds,spend_period,spend_amount_microusd,deleted_at IS NULL,notify_emails FROM alert_rules ORDER BY name,spend_period",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let find = |name: &str| rules.iter().filter(|r| r.2 == name).collect::<Vec<_>>();
    // Installation-only rule: converted in place for its shortest period (the
    // zero "week" budget never alerted and is not a reference amount).
    let o = find("Installation budget");
    assert_eq!(o.len(), 1);
    assert_eq!(
        (
            o[0].0,
            o[0].1.as_str(),
            o[0].3.clone(),
            o[0].4.clone(),
            o[0].5.as_deref(),
            o[0].6,
            o[0].8.clone()
        ),
        (
            only,
            "spend_threshold",
            None,
            Some(vec![50, 80, 100]),
            Some("day"),
            Some(500_000),
            vec!["ops@example.test".to_owned()]
        )
    );
    let extra = find("Installation budget (installation monthly spend)");
    assert_eq!(
        (
            extra[0].1.as_str(),
            extra[0].5.as_deref(),
            extra[0].6,
            extra[0].4.clone()
        ),
        (
            "spend_threshold",
            Some("month"),
            Some(10_000_000),
            Some(vec![50, 80, 100])
        )
    );
    // Mixed rule: keeps its other layers; installation part became spend rules.
    let m = find("Budgets");
    assert_eq!(
        (m[0].0, m[0].3.clone()),
        (mixed, Some(vec!["local".to_owned(), "key".to_owned()]))
    );
    assert_eq!(
        find("Budgets (installation daily spend)")[0].6,
        Some(500_000)
    );
    assert_eq!(
        find("Budgets (installation monthly spend)")[0].6,
        Some(10_000_000)
    );
    // Soft-deleted rules keep their stored configuration (history).
    let d = find("Old");
    assert_eq!(
        (d[0].1.as_str(), d[0].3.clone(), d[0].7),
        (
            "budget_threshold",
            Some(vec!["installation".to_owned()]),
            false
        )
    );
    // Open incidents: the in-place rule's day incident continues (same
    // subject); the others were superseded without a "resolved" email.
    let events: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT rule_id,subject_key,resolution FROM alert_events ORDER BY rule_id,subject_key",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(events.contains(&(only, "installation:day".into(), None)));
    assert!(events.contains(&(only, "installation:month".into(), Some("superseded".into()))));
    assert!(events.contains(&(
        mixed,
        "installation:month".into(),
        Some("superseded".into())
    )));
    let resolved_emails: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_deliveries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resolved_emails, 0);
    // Audited: the removed installation policy and each changed rule.
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_events ORDER BY action")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        actions,
        [
            "alert_rule.installation_layer_removed",
            "alert_rule.installation_layer_removed",
            "policy.installation_removed"
        ]
    );
    // Installation limits are unrepresentable afterwards.
    assert_eq!(
        count_pool(&pool, "SELECT count(*) FROM policy_budgets").await,
        0
    );
    assert!(
        sqlx::query("SELECT 1 FROM installation_policy")
            .execute(&pool)
            .await
            .is_err()
    );
}
async fn count_pool(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// 0036: one workspace or key lineage near its scoped-admission ceiling.
/// Sustained rate means every complete minute of the window; lock-wait p95
/// comes from the replicas' flushed wait counts. Shared scopes are named
/// (workspace reference); personal workspaces only as "a personal
/// workspace"; keys never by id or name. Email: Platform Admins only.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn admission_ceiling_fires_per_scope_and_names_only_shared_workspaces(pool: PgPool) {
    let s = seed(pool).await;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT date_trunc('minute',clock_timestamp())")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let minute = |i: i64| now - TimeDelta::minutes(i);
    // (scope kind, id, requests per minute, minutes back that have them).
    let rows: [(&str, Uuid, i64, Vec<i64>); 4] = [
        ("workspace", s.team, 13_000, (0..=7).collect()),
        ("key", s.key, 13_000, vec![1, 4]),
        ("workspace", s.personal, 1_000, (0..=7).collect()),
        ("key", s.personal_key, 1_000, (0..=7).collect()),
    ];
    for (kind, id, n, minutes) in &rows {
        for i in minutes {
            sqlx::query("INSERT INTO rate_minute_counters(minute_start,scope_kind,scope_id,requests) VALUES($1,$2,$3,$4)")
                .bind(minute(*i)).bind(kind).bind(id).bind(n).execute(&s.pool).await.unwrap();
        }
    }
    // 300 of the personal scope's ~5,000 admissions waited 300 ms (6 %):
    // p95 >= 250 ms. 20 of the team key's waited 2.6 s (too few to count).
    crate::governance::pressure::reset();
    for _ in 0..300 {
        crate::governance::pressure::record(
            minute(2) + TimeDelta::seconds(7),
            s.personal,
            s.personal_key,
            std::time::Duration::from_millis(300),
        );
    }
    for _ in 0..20 {
        crate::governance::pressure::record(
            minute(2),
            s.team,
            s.key,
            std::time::Duration::from_millis(2600),
        );
    }
    assert_eq!(
        crate::governance::pressure::flush(&s.store).await.unwrap(),
        4
    );
    // A second flush adds (replicas are additive), the first one is kept.
    crate::governance::pressure::record(
        minute(2),
        s.team,
        s.key,
        std::time::Duration::from_millis(12),
    );
    crate::governance::pressure::flush(&s.store).await.unwrap();
    let waits: Vec<i64> = sqlx::query_scalar(
        "SELECT waits FROM admission_lock_waits WHERE scope_kind='workspace' AND scope_id=$1",
    )
    .bind(s.team)
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(waits, [21, 20, 20, 20, 20, 20, 20, 20]);
    rule(&s, "scope,kind,window_minutes,ceiling_requests_per_second,ceiling_lock_wait_ms,notify_platform_admins", "'installation','admission_ceiling',5,200,250,true").await;
    assert_eq!(evaluate(&s).await.fired, 3);
    let open = open(&s).await;
    let mut expected = vec![
        format!("key:{}", s.personal_key),
        format!("workspace:{}", s.personal),
        format!("workspace:{}", s.team),
    ];
    expected.sort();
    let subjects: Vec<String> = open.iter().map(|o| o.0.clone()).collect();
    assert_eq!(subjects, expected);
    let events: Vec<(String, Option<Uuid>, String, Value)> = sqlx::query_as(
        "SELECT subject_key,workspace_id,summary,details FROM alert_events ORDER BY subject_key",
    )
    .fetch_all(&s.pool)
    .await
    .unwrap();
    let team_subject = format!("workspace:{}", s.team);
    let team = events.iter().find(|e| e.0 == team_subject).unwrap();
    assert_eq!(team.1, Some(s.team));
    assert_eq!(team.2, "A workspace is near its admission ceiling");
    assert_eq!(team.3["workspace_name"], "Platform team");
    assert_eq!(team.3["by_rate"], true);
    assert_eq!(team.3["admissions_per_second"], "216.6");
    for (subject, ws, summary, details) in events.iter().filter(|e| e.0 != team_subject) {
        assert_eq!(*ws, None, "{subject}");
        assert!(summary.contains("personal workspace"), "{summary}");
        assert_eq!(details["personal"], true);
        assert_eq!(details["workspace_name"], Value::Null);
        assert_eq!(details["by_lock_wait"], true);
        assert_eq!(details["lock_wait_p95_at_least_ms"], 250);
        let text = details.to_string();
        assert!(
            !text.contains(&s.personal.to_string()) && !text.contains(&s.personal_key.to_string())
        );
        assert!(!text.contains("Private key name") && !text.contains("owner@example.test"));
    }
    // Idempotent while the condition holds.
    assert_eq!(evaluate(&s).await.fired, 0);
    // Email: Platform Admins only; the team is named, the personal workspace is not.
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    assert_eq!(deliver_pending(&s.store, 10).await, 3);
    let mut bodies = Vec::new();
    for _ in 0..3 {
        let got = rx.recv().await.unwrap();
        let to = got.rcpt_to.join(",");
        assert!(
            to.contains("admin@example.test") && !to.contains("owner"),
            "{to}"
        );
        assert!(!got.data.contains("Secret key name") && !got.data.contains("Private key name"));
        bodies.push(got.data);
    }
    assert!(bodies.iter().any(|b| b.contains("Team Platform team")));
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b.contains("personal workspace"))
            .count(),
        2
    );
    // Traffic drops: everything resolves.
    sqlx::query("UPDATE rate_minute_counters SET requests=1")
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 3);
}

#[test]
fn lock_wait_p95_is_the_largest_bound_five_percent_reached() {
    assert_eq!(
        p95_at_least_ms(&[100, 60, 50, 49, 0, 0, 0, 0], 1000),
        Some(50)
    );
    assert_eq!(p95_at_least_ms(&[49, 0, 0, 0, 0, 0, 0, 0], 1000), None);
    assert_eq!(p95_at_least_ms(&[1; 8], 0), None);
    assert_eq!(p95_at_least_ms(&[1; 8], 20), Some(2500));
}
