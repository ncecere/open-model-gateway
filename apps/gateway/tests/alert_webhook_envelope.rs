#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
//! The signed bytes are the queued bytes: the outbox guard accepts exactly the
//! envelope the one SQL constructor builds, and only for eligible events.
use sqlx::PgPool;
use uuid::Uuid;

async fn receiver(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('review','https://example.test/private','env:HOOK',true) RETURNING id")
        .fetch_one(pool).await.unwrap()
}
async fn scim_event(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin',gen_random_uuid()::text,1,'critical','safe') RETURNING id")
        .fetch_one(pool).await.unwrap()
}
/// An independent oracle for the test envelope's canonical text.
async fn canonical_test(pool: &PgPool, delivery: Uuid) -> String {
    sqlx::query_scalar("SELECT jsonb_build_object('schema','omg.alert.v1','installation_id',(SELECT id FROM installation WHERE singleton),'delivery_id',$1::uuid,'test',true,'event',NULL)::text")
        .bind(delivery).fetch_one(pool).await.unwrap()
}
/// The best forgery an attacker with the insert grant can build: the exact
/// canonical recipe applied to any event, eligible or not.
async fn forged(pool: &PgPool, delivery: Uuid, event: Uuid, transition: &str) -> Vec<u8> {
    sqlx::query_scalar("SELECT convert_to(jsonb_build_object('schema','omg.alert.v1','installation_id',(SELECT id FROM installation WHERE singleton),'delivery_id',$1::uuid,'test',false,'event',jsonb_build_object('id',e.id,'transition',$3::text,'kind',e.kind,'severity',e.severity,'occurred_at',to_char((CASE WHEN $3::text='resolved' THEN coalesce(e.resolved_at,e.fired_at) ELSE e.fired_at END) AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),'summary',coalesce(alert_webhook_summary(e.kind),'forged')))::text,'UTF8') FROM alert_events e WHERE e.id=$2")
        .bind(delivery).bind(event).bind(transition).fetch_one(pool).await.unwrap()
}
fn message(error: &sqlx::Error) -> (String, String) {
    let error = error.as_database_error().unwrap();
    (
        error.code().unwrap().into_owned(),
        error.message().to_owned(),
    )
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn test_envelope_must_be_the_exact_canonical_bytes(pool: PgPool) {
    let dest = receiver(&pool).await;
    let id = Uuid::new_v4();
    let canonical = canonical_test(&pool, id).await;
    let insert =
        "INSERT INTO alert_webhook_outbox(id,destination_id,test,payload) VALUES($1,$2,true,$3)";
    let compact = canonical.replace(": ", ":").replace(", ", ",");
    let envelope: serde_json::Value = serde_json::from_str(&canonical).unwrap();
    for (name, payload) in [
        // jsonb keeps the last duplicate, so it compares equal as jsonb.
        (
            "duplicate key",
            format!("{{\"schema\": \"stale\", {}", &canonical[1..]),
        ),
        ("compact whitespace", compact),
        ("trailing newline", format!("{canonical}\n")),
        ("leading space", format!(" {canonical}")),
        (
            "different key order",
            serde_json::to_string(&envelope).unwrap(),
        ),
        (
            "pretty printed",
            serde_json::to_string_pretty(&envelope).unwrap(),
        ),
    ] {
        let result = sqlx::query(insert)
            .bind(id)
            .bind(dest)
            .bind(payload.as_bytes())
            .execute(&pool)
            .await;
        let error = result.expect_err(&format!("{name} payload was queued"));
        assert_eq!(
            message(&error),
            ("23514".into(), "invalid webhook test envelope".into()),
            "{name}"
        );
    }
    // The delivery id in the body is the row id: another id's bytes are refused.
    let other = canonical_test(&pool, Uuid::new_v4()).await;
    assert!(
        sqlx::query(insert)
            .bind(id)
            .bind(dest)
            .bind(other.as_bytes())
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query(insert)
        .bind(id)
        .bind(dest)
        .bind(canonical.as_bytes())
        .execute(&pool)
        .await
        .expect("the canonical bytes are accepted");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn event_payloads_must_be_the_exact_canonical_bytes(pool: PgPool) {
    let dest = receiver(&pool).await;
    let event = scim_event(&pool).await;
    let id = Uuid::new_v4();
    let insert = "INSERT INTO alert_webhook_outbox(id,destination_id,event_id,transition,payload) VALUES($1,$2,$3,'fired',$4)";
    let try_insert = |id: Uuid, payload: Vec<u8>| {
        let pool = pool.clone();
        async move {
            sqlx::query(insert)
                .bind(id)
                .bind(dest)
                .bind(event)
                .bind(payload)
                .execute(&pool)
                .await
        }
    };
    for payload in [
        b"{}".to_vec(),
        br#"{"anything":"here"}"#.to_vec(),
        b" ".to_vec(),
        b"PRIVATE-CANARY".to_vec(),
    ] {
        let error = try_insert(id, payload)
            .await
            .expect_err("an arbitrary non-test payload was queued");
        assert_eq!(
            message(&error),
            ("23514".into(), "invalid webhook payload".into())
        );
    }
    let canonical = forged(&pool, id, event, "fired").await;
    let text = String::from_utf8(canonical.clone()).unwrap();
    for (name, payload) in [
        ("trailing newline", format!("{text}\n")),
        ("compact", text.replace(": ", ":").replace(", ", ",")),
        (
            "duplicate key",
            format!("{{\"schema\": \"stale\", {}", &text[1..]),
        ),
    ] {
        assert!(
            try_insert(id, payload.into_bytes()).await.is_err(),
            "{name} event payload was queued"
        );
    }
    // A different delivery id's bytes do not match the row id.
    assert!(
        try_insert(id, forged(&pool, Uuid::new_v4(), event, "fired").await)
            .await
            .is_err()
    );
    // The envelope's transition is bound to the row's transition.
    assert!(
        try_insert(id, forged(&pool, id, event, "resolved").await)
            .await
            .is_err()
    );
    try_insert(id, canonical)
        .await
        .expect("the canonical bytes are accepted");
    let constructor: String = sqlx::query_scalar("SELECT alert_webhook_payload($1,$2,'fired')")
        .bind(id)
        .bind(event)
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT payload FROM alert_webhook_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, constructor.as_bytes());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn runtime_role_cannot_queue_excluded_events_or_forged_payloads(pool: PgPool) {
    let dest = receiver(&pool).await;
    let role = format!("w45a_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE ROLE {role}; GRANT USAGE ON SCHEMA public TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {role}; GRANT INSERT(id,event_id,transition,destination_id,test,payload) ON alert_webhook_outbox TO {role};")).execute(&pool).await.unwrap();
    let owner = Uuid::new_v4();
    let personal = Uuid::new_v4();
    let team = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'owner@example.test')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    for (id, kind) in [(personal, "personal"), (team, "team")] {
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'ws',$2,$3)")
            .bind(id)
            .bind(kind)
            .bind((kind == "personal").then_some(owner))
            .execute(&pool)
            .await
            .unwrap();
    }
    let (install, workspace, silent, ceiling) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    for (id, scope, ws, notify) in [
        (install, "installation", None, true),
        (workspace, "workspace", Some(team), true),
        (silent, "installation", None, false),
    ] {
        sqlx::query("INSERT INTO alert_rules(id,scope,workspace_id,kind,name,spike_factor_percent,min_spend_microusd,notify_platform_admins) VALUES($1,$2,$3,'spend_spike','rule',200,1,$4)").bind(id).bind(scope).bind(ws).bind(notify).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO alert_rules(id,scope,kind,name,window_minutes,ceiling_requests_per_second,notify_platform_admins) VALUES($1,'installation','admission_ceiling','ceiling',5,1,true)").bind(ceiling).execute(&pool).await.unwrap();
    let mut excluded: Vec<(&str, Uuid, &str)> = Vec::new();
    for (name, rule, builtin, kind, ws) in [
        ("workspace rule", Some(workspace), None, "spend_spike", Some(team)),
        ("rule not notifying admins", Some(silent), None, "spend_spike", None),
        ("personal workspace", Some(install), None, "spend_spike", Some(personal)),
        ("ceiling", Some(ceiling), None, "admission_ceiling", Some(team)),
        ("personal budget", None, Some("personal_budget"), "budget_threshold", Some(personal)),
    ] {
        let event = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,workspace_id,summary) VALUES($1,$2,$3,$4,$1::text,1,'warning',$5,'safe')").bind(event).bind(rule).bind(builtin).bind(kind).bind(ws).execute(&pool).await.unwrap();
        excluded.push((name, event, "fired"));
    }
    let key = Uuid::new_v4();
    let reminder = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,expires_at) VALUES($1,$2,$3,'key',sha256($1::text::bytea),now()+interval '1 day')").bind(key).bind(personal).bind(owner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO alert_events(id,kind,subject_key,level,severity,workspace_id,summary,api_key_id,key_expires_at,threshold_days) SELECT $1,'key_expiring',$2::text,1,'warning',$3,'safe',$2,expires_at,1 FROM api_keys WHERE id=$2").bind(reminder).bind(key).bind(personal).execute(&pool).await.unwrap();
    excluded.push(("key reminder", reminder, "fired"));
    for resolution in ["superseded", "rule_disabled"] {
        let event = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary,resolved_at,resolution) VALUES($1,$2,'spend_spike',$1::text,1,'warning','safe',now(),$3)").bind(event).bind(install).bind(resolution).execute(&pool).await.unwrap();
        excluded.push(("non-cleared resolution", event, "resolved"));
    }
    let unresolved = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary) VALUES($1,$2,'spend_spike',$1::text,1,'warning','safe')").bind(unresolved).bind(install).execute(&pool).await.unwrap();
    excluded.push(("resolved before cleared", unresolved, "resolved"));
    let insert = "INSERT INTO alert_webhook_outbox(id,event_id,transition,destination_id,payload) VALUES($1,$2,$3,$4,$5)";
    let as_role = |id: Uuid, event: Uuid, transition: String, payload: Vec<u8>| {
        let (pool, role) = (pool.clone(), role.clone());
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(&format!("SET LOCAL ROLE {role}"))
                .execute(&mut *tx)
                .await
                .unwrap();
            let result = sqlx::query(insert)
                .bind(id)
                .bind(event)
                .bind(transition)
                .bind(dest)
                .bind(payload)
                .execute(&mut *tx)
                .await;
            if result.is_ok() {
                tx.commit().await.unwrap();
            }
            result
        }
    };
    for (name, event, transition) in excluded {
        let id = Uuid::new_v4();
        // The exact canonical recipe, so only the eligibility rules can refuse it.
        let payload = forged(&pool, id, event, transition).await;
        let error = as_role(id, event, transition.into(), payload)
            .await
            .expect_err(&format!("runtime queued an excluded event: {name}"));
        assert_eq!(
            message(&error),
            ("23514".into(), "invalid webhook payload".into()),
            "{name}"
        );
    }
    // Control: the same recipe is accepted for an eligible event, and arbitrary
    // bytes are refused for it.
    let eligible = scim_event(&pool).await;
    let id = Uuid::new_v4();
    assert!(
        as_role(id, eligible, "fired".into(), b"{}".to_vec())
            .await
            .is_err()
    );
    let payload = forged(&pool, id, eligible, "fired").await;
    as_role(id, eligible, "fired".into(), payload)
        .await
        .expect("eligible events queue through the same recipe");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    sqlx::raw_sql(&format!("DROP OWNED BY {role}; DROP ROLE {role};"))
        .execute(&pool)
        .await
        .unwrap();
}
