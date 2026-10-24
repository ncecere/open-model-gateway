#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_projection_every_exclusion_and_transition(pool: PgPool) {
    let owner = Uuid::new_v4();
    let personal = Uuid::new_v4();
    let team = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'PERSONAL-CANARY@example.test')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    for (id, kind) in [(personal, "personal"), (team, "team")] {
        sqlx::query(
            "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'PERSONAL-CANARY',$2,$3)",
        )
        .bind(id)
        .bind(kind)
        .bind(if kind == "personal" {
            Some(owner)
        } else {
            None
        })
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('on','https://example.test/hook','env:HOOK',true),('off','https://example.test/off','env:HOOK',false)").execute(&pool).await.unwrap();
    let install = Uuid::new_v4();
    let workspace = Uuid::new_v4();
    let silent = Uuid::new_v4();
    let ceiling = Uuid::new_v4();
    for (id, scope, ws, notify) in [
        (install, "installation", None, true),
        (workspace, "workspace", Some(team), true),
        (silent, "installation", None, false),
    ] {
        sqlx::query("INSERT INTO alert_rules(id,scope,workspace_id,kind,name,spike_factor_percent,min_spend_microusd,notify_platform_admins) VALUES($1,$2,$3,'spend_spike','PERSONAL-CANARY',200,1,$4)").bind(id).bind(scope).bind(ws).bind(notify).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO alert_rules(id,scope,kind,name,window_minutes,ceiling_requests_per_second,notify_platform_admins) VALUES($1,'installation','admission_ceiling','ceiling',5,1,true)").bind(ceiling).execute(&pool).await.unwrap();
    for (rule, builtin, kind, ws, expected) in [
        (Some(install), None, "spend_spike", None, 1_i64),
        (Some(install), None, "spend_spike", Some(team), 1),
        (Some(install), None, "spend_spike", Some(personal), 0),
        (Some(workspace), None, "spend_spike", Some(team), 0),
        (Some(silent), None, "spend_spike", None, 0),
        (Some(ceiling), None, "admission_ceiling", None, 0),
        (Some(ceiling), None, "admission_ceiling", Some(team), 0),
        (Some(ceiling), None, "admission_ceiling", Some(personal), 0),
        (
            None,
            Some("personal_budget"),
            "budget_threshold",
            Some(personal),
            0,
        ),
        (None, Some("scim_last_admin"), "scim_last_admin", None, 1),
        (
            None,
            Some("partitions_missing"),
            "partitions_missing",
            None,
            1,
        ),
        (None, Some("history_orphans"), "history_orphans", None, 1),
    ] {
        let event = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,workspace_id,summary,details) VALUES($1,$2,$3,$4,$1::text,1,'warning',$5,'PERSONAL-CANARY','{\"owner\":\"PERSONAL-CANARY\"}')").bind(event).bind(rule).bind(builtin).bind(kind).bind(ws).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')").bind(event).execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1"
            )
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
            expected,
            "{kind} {builtin:?} {ws:?}"
        );
        sqlx::query("UPDATE alert_events SET resolved_at=clock_timestamp(),resolution='cleared' WHERE id=$1").bind(event).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'resolved')").bind(event).execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1"
            )
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
            expected * 2
        );
    }
    // All key reminders, including shared human and service keys, are excluded.
    for (ws, service) in [(personal, false), (team, false), (team, true)] {
        let key = Uuid::new_v4();
        let event = Uuid::new_v4();
        let sa = Uuid::new_v4();
        if service {
            sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'PERSONAL-CANARY')").bind(sa).bind(ws).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,service_account_id,name,secret_hash,expires_at) VALUES($1,$2,$3,$4,'PERSONAL-CANARY',sha256($1::text::bytea),now()+interval '1 day')").bind(key).bind(ws).bind(if service {None} else {Some(owner)}).bind(service.then_some(sa)).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_events(id,kind,subject_key,level,severity,workspace_id,summary,api_key_id,key_expires_at,threshold_days) SELECT $1,'key_expiring',$2::text,1,'warning',$3,'PERSONAL-CANARY',$2,expires_at,1 FROM api_keys WHERE id=$2").bind(event).bind(key).bind(ws).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')").bind(event).execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1"
            )
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
    for resolution in ["superseded", "rule_disabled"] {
        let event = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary,resolved_at,resolution) VALUES($1,$2,'spend_spike',$1::text,1,'warning','PERSONAL-CANARY',now(),$3)").bind(event).bind(install).bind(resolution).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'resolved')").bind(event).execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1"
            )
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
    for bytes in sqlx::query_scalar::<_, Vec<u8>>("SELECT payload FROM alert_webhook_outbox")
        .fetch_all(&pool)
        .await
        .unwrap()
    {
        let payload = String::from_utf8(bytes).unwrap();
        for canary in [
            "PERSONAL-CANARY",
            &personal.to_string(),
            &team.to_string(),
            &owner.to_string(),
            "subject_key",
            "details",
            "workspace_id",
        ] {
            assert!(!payload.contains(canary));
        }
    }
}
