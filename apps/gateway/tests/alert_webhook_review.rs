#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::PgPool;
use uuid::Uuid;

async fn receiver(pool: &PgPool, enabled: bool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('review','https://example.test/private','env:HOOK',$1) RETURNING id")
        .bind(enabled).fetch_one(pool).await.unwrap()
}
async fn event(pool: &PgPool, builtin: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),$1,$1,gen_random_uuid()::text,1,'critical','safe') RETURNING id")
        .bind(builtin).fetch_one(pool).await.unwrap()
}
/// What the current transaction has read and inserted so far, per table.
async fn touched(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Vec<(String, i64, i64)> {
    sqlx::query_as("SELECT relname::text,coalesce(seq_scan,0)+coalesce(idx_scan,0),n_tup_ins FROM pg_stat_xact_user_tables WHERE relname IN ('alert_webhook_destinations','alert_webhook_outbox','alert_rules','workspaces','installation') ORDER BY 1").fetch_all(&mut **tx).await.unwrap()
}
/// The counters this statement added: the view also carries this backend's
/// not yet flushed counts from earlier transactions, so take a baseline.
async fn delivery_touches(pool: &PgPool, event: Uuid) -> Vec<(String, i64, i64)> {
    let mut tx = pool.begin().await.unwrap();
    let before = touched(&mut tx).await;
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&mut *tx)
        .await
        .unwrap();
    let after = touched(&mut tx).await;
    tx.rollback().await.unwrap();
    after
        .into_iter()
        .map(|(table, scans, inserts)| {
            let base = before.iter().find(|row| row.0 == table);
            (
                table,
                scans - base.map_or(0, |row| row.1),
                inserts - base.map_or(0, |row| row.2),
            )
        })
        .collect()
}
const DELIVERY: &str = "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired') RETURNING id";

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_normal_attempts_increment_exactly_once_through_five(pool: PgPool) {
    let dest = receiver(&pool, true).await;
    let event = event(&pool, "scim_last_admin").await;
    let id:Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,event_id,transition,payload) SELECT i,$1,(SELECT enable_generation FROM alert_webhook_destinations WHERE id=$1),$2,'fired',convert_to(alert_webhook_payload(i,$2,'fired'),'UTF8') FROM (SELECT gen_random_uuid() AS i) x RETURNING id").bind(dest).bind(event).fetch_one(&pool).await.unwrap();
    let update = "UPDATE alert_webhook_outbox SET attempt_count=$2 WHERE id=$1";
    let error = sqlx::query(update)
        .bind(id)
        .bind(2_i32)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("P0001")
    );
    assert_eq!(
        error.as_database_error().unwrap().message(),
        "immutable webhook payload, identity or terminal outcome"
    );
    sqlx::query(update)
        .bind(id)
        .bind(1_i32)
        .execute(&pool)
        .await
        .unwrap();
    let error = sqlx::query(update)
        .bind(id)
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("P0001")
    );
    for attempt in 2..=5_i32 {
        sqlx::query(update)
            .bind(id)
            .bind(attempt)
            .execute(&pool)
            .await
            .unwrap();
    }
    let error = sqlx::query(update)
        .bind(id)
        .bind(6_i32)
        .execute(&pool)
        .await
        .unwrap_err();
    let error = error.as_database_error().unwrap();
    assert_eq!(error.code().as_deref(), Some("23514"));
    assert_eq!(
        error.constraint(),
        Some("alert_webhook_outbox_attempt_count_check")
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_enqueue_does_not_wait_for_destination_management(pool: PgPool) {
    let dest = receiver(&pool, true).await;
    let event = event(&pool, "scim_last_admin").await;
    let mut management = pool.begin().await.unwrap();
    sqlx::query("SELECT lock_installation()")
        .execute(&mut *management)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET label='held' WHERE id=$1")
        .bind(dest)
        .execute(&mut *management)
        .await
        .unwrap();
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL statement_timeout='1s'")
        .execute(&mut *enqueue)
        .await
        .unwrap();
    let delivery = sqlx::query_scalar::<_, Uuid>(DELIVERY)
        .bind(event)
        .fetch_one(&mut *enqueue)
        .await;
    assert!(
        delivery.is_ok(),
        "webhook enqueue must not wait on destination management: {delivery:?}"
    );
    enqueue.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    management.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_enqueue_failures_never_abort_email_or_builtin_delivery(pool: PgPool) {
    receiver(&pool, true).await;
    // A distinct disposable role avoids shared gateway_runtime role races.
    let role = format!("w45a_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE ROLE {role}; GRANT USAGE ON SCHEMA public TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {role}; GRANT INSERT ON alert_deliveries,alert_recipient_outbox TO {role}; GRANT UPDATE(status,recipients,sent,completed_at,initialized) ON alert_deliveries TO {role}; GRANT UPDATE(state) ON alert_recipient_outbox TO {role}; REVOKE SELECT ON alert_webhook_destinations,alert_webhook_outbox FROM {role};")).execute(&pool).await.unwrap();
    for builtin in ["scim_last_admin", "partitions_missing", "history_orphans"] {
        let event = event(&pool, builtin).await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let delivery = sqlx::query_scalar::<_, Uuid>(DELIVERY)
            .bind(event)
            .fetch_one(&mut *tx)
            .await;
        assert!(
            delivery.is_ok(),
            "unreadable webhooks aborted {builtin}: {delivery:?}"
        );
        let delivery = delivery.unwrap();
        sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) VALUES($1,sha256('email'::bytea))").bind(delivery).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE alert_deliveries SET initialized=true WHERE id=$1")
            .bind(delivery)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE alert_recipient_outbox SET state='sent' WHERE delivery_id=$1")
            .bind(delivery)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE alert_deliveries SET status='sent',recipients=1,sent=1,completed_at=clock_timestamp() WHERE id=$1").bind(delivery).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM alert_deliveries WHERE id=$1")
                .bind(delivery)
                .fetch_one(&pool)
                .await
                .unwrap(),
            "sent"
        );
    }
    sqlx::raw_sql(&format!("DROP OWNED BY {role}; DROP ROLE {role};"))
        .execute(&pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_forced_enqueue_failure_never_aborts_builtin_delivery(pool: PgPool) {
    receiver(&pool, true).await;
    sqlx::raw_sql("CREATE FUNCTION force_webhook_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'PRIVATE-CANARY'; END $$; CREATE TRIGGER forced BEFORE INSERT ON alert_webhook_outbox FOR EACH ROW EXECUTE FUNCTION force_webhook_failure();").execute(&pool).await.unwrap();
    for builtin in ["scim_last_admin", "partitions_missing", "history_orphans"] {
        let event = event(&pool, builtin).await;
        let delivery = sqlx::query_scalar::<_, Uuid>(DELIVERY)
            .bind(event)
            .fetch_one(&pool)
            .await;
        assert!(
            delivery.is_ok(),
            "forced webhook error aborted {builtin}: {delivery:?}"
        );
        let delivery = delivery.unwrap();
        sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) VALUES($1,sha256('email'::bytea))").bind(delivery).execute(&pool).await.unwrap();
        sqlx::query("UPDATE alert_deliveries SET initialized=true WHERE id=$1")
            .bind(delivery)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE alert_recipient_outbox SET state='sent' WHERE delivery_id=$1")
            .bind(delivery)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE alert_deliveries SET status='sent',recipients=1,sent=1,completed_at=clock_timestamp() WHERE id=$1").bind(delivery).execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM alert_deliveries WHERE id=$1")
                .bind(delivery)
                .fetch_one(&pool)
                .await
                .unwrap(),
            "sent"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_disabled_fast_path_never_reads_event_projection(pool: PgPool) {
    receiver(&pool, false).await;
    let event = event(&pool, "scim_last_admin").await;
    let role = format!("w45a_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE ROLE {role}; GRANT USAGE ON SCHEMA public TO {role}; GRANT SELECT ON alert_webhook_destinations,alert_deliveries TO {role}; GRANT INSERT ON alert_deliveries TO {role};")).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query(DELIVERY)
            .bind(event)
            .execute(&mut *tx)
            .await
            .is_ok(),
        "disabled webhooks must not require alert_events SELECT"
    );
    tx.commit().await.unwrap();
    sqlx::raw_sql(&format!("DROP OWNED BY {role}; DROP ROLE {role};"))
        .execute(&pool)
        .await
        .unwrap();
    // Behavioral: with no enabled destination the trigger reads the
    // destination list and nothing else (no event projection, no rule,
    // workspace or installation read) and writes no outbox row. The
    // transaction-local statistics measure what the statement touched.
    let fresh = self::event(&pool, "scim_last_admin").await;
    let stats = delivery_touches(&pool, fresh).await;
    let scans = |stats: &[(String, i64, i64)], table: &str| {
        stats
            .iter()
            .find(|row| row.0 == table)
            .map_or(0, |row| row.1)
    };
    assert!(
        scans(&stats, "alert_webhook_destinations") > 0,
        "control: the destination list is read: {stats:?}"
    );
    for table in [
        "alert_webhook_outbox",
        "alert_rules",
        "workspaces",
        "installation",
    ] {
        assert_eq!(
            scans(&stats, table),
            0,
            "{table} read on the fast path: {stats:?}"
        );
    }
    assert!(stats.iter().all(|row| row.2 == 0), "{stats:?}");
    // Control: once a destination is enabled the same measurement sees the work.
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    let fresh = self::event(&pool, "scim_last_admin").await;
    let stats = delivery_touches(&pool, fresh).await;
    assert!(
        stats
            .iter()
            .any(|row| row.0 == "alert_webhook_outbox" && row.2 == 1),
        "control: an outbox row is inserted: {stats:?}"
    );
    let definition: String =
        sqlx::query_scalar("SELECT pg_get_functiondef('alert_webhook_enqueue()'::regprocedure)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !definition.contains("SQLERRM"),
        "warning must not contain error text"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_insert_rechecks_destination_and_fixed_test_shape(pool: PgPool) {
    let dest = receiver(&pool, false).await;
    let installation: Uuid = sqlx::query_scalar("SELECT id FROM installation")
        .fetch_one(&pool)
        .await
        .unwrap();
    let insert = "INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) VALUES($1,$2,(SELECT enable_generation FROM alert_webhook_destinations WHERE id=$2),true,$3)";
    let id = Uuid::new_v4();
    let envelope = serde_json::json!({"schema":"omg.alert.v1","installation_id":installation,"delivery_id":id,"test":true,"event":null});
    let canonical = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT alert_webhook_payload($1,NULL,NULL)")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
                .into_bytes()
        }
    };
    let bytes = canonical(id).await;
    let normal_event = event(&pool, "scim_last_admin").await;
    for destination in [dest, Uuid::new_v4()] {
        let error = sqlx::query(insert)
            .bind(id)
            .bind(destination)
            .bind(&bytes)
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
        assert_eq!(
            error.as_database_error().unwrap().message(),
            "webhook destination is not eligible"
        );
        let error = sqlx::query("INSERT INTO alert_webhook_outbox(destination_id,destination_generation,event_id,transition,payload) VALUES($1,0,$2,'fired','{}'::bytea)").bind(destination).bind(normal_event).execute(&pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
        assert_eq!(
            error.as_database_error().unwrap().message(),
            "webhook destination is not eligible"
        );
    }
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    for field in [
        "schema",
        "installation_id",
        "delivery_id",
        "test",
        "event",
        "extra",
    ] {
        let mut bad = envelope.clone();
        bad[field] = serde_json::json!("PRIVATE-CANARY");
        let error = sqlx::query(insert)
            .bind(id)
            .bind(dest)
            .bind(serde_json::to_vec(&bad).unwrap())
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().message(),
            "invalid webhook test envelope"
        );
    }
    for field in ["schema", "installation_id", "delivery_id", "test", "event"] {
        let mut bad = envelope.clone();
        bad.as_object_mut().unwrap().remove(field);
        let error = sqlx::query(insert)
            .bind(id)
            .bind(dest)
            .bind(serde_json::to_vec(&bad).unwrap())
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().message(),
            "invalid webhook test envelope"
        );
    }
    sqlx::query(insert)
        .bind(id)
        .bind(dest)
        .bind(&bytes)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_outbox SET attempt_count=1 WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let error = sqlx::query("UPDATE alert_webhook_outbox SET attempt_count=2 WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    assert_eq!(
        error.as_database_error().unwrap().constraint(),
        Some("alert_webhook_outbox_attempt_count_check")
    );
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=false,deleted_at=clock_timestamp() WHERE id=$1").bind(dest).execute(&pool).await.unwrap();
    assert!(
        sqlx::query(insert)
            .bind(id)
            .bind(dest)
            .bind(&bytes)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_soft_delete_scrubs_only_on_final_transition(pool: PgPool) {
    let dest = receiver(&pool, true).await;
    sqlx::query("UPDATE alert_webhook_destinations SET previous_secret_ref='env:OLD',previous_secret_until=clock_timestamp()+interval '1 hour' WHERE id=$1").bind(dest).execute(&pool).await.unwrap();
    assert!(
        sqlx::query("UPDATE alert_webhook_destinations SET url=NULL WHERE id=$1")
            .bind(dest)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE alert_webhook_destinations SET label='changed',deleted_at=clock_timestamp(),enabled=false WHERE id=$1").bind(dest).execute(&pool).await.is_err());
    sqlx::query("UPDATE alert_webhook_destinations SET deleted_at=clock_timestamp(),enabled=false WHERE id=$1").bind(dest).execute(&pool).await.unwrap();
    let scrubbed:bool=sqlx::query_scalar("SELECT url IS NULL AND current_secret_ref IS NULL AND previous_secret_ref IS NULL AND previous_secret_until IS NULL FROM alert_webhook_destinations WHERE id=$1").bind(dest).fetch_one(&pool).await.unwrap();
    assert!(
        scrubbed,
        "deleted receiver retains secret-bearing URL or references"
    );
    for field in [
        "url='https://example.test/restored'",
        "current_secret_ref='env:RESTORED'",
        "enabled=true",
        "deleted_at=NULL",
    ] {
        assert!(
            sqlx::query(&format!(
                "UPDATE alert_webhook_destinations SET {field} WHERE id=$1"
            ))
            .bind(dest)
            .execute(&pool)
            .await
            .is_err()
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_status_has_matching_partial_indexes(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (query, index) in [
        (
            "SELECT count(*) FROM alert_webhook_outbox WHERE destination_id=$1 AND state='waiting'",
            "alert_webhook_outbox_waiting",
        ),
        (
            "SELECT count(*) FROM alert_webhook_outbox WHERE destination_id=$1 AND state='exhausted'",
            "alert_webhook_outbox_dead_letters",
        ),
        (
            "SELECT completed_at FROM alert_webhook_outbox WHERE destination_id=$1 AND state='sent' ORDER BY completed_at DESC LIMIT 1",
            "alert_webhook_outbox_success",
        ),
        (
            "SELECT last_error FROM alert_webhook_outbox WHERE destination_id=$1 AND last_error IS NOT NULL ORDER BY created_at DESC,id DESC LIMIT 1",
            "alert_webhook_outbox_error",
        ),
    ] {
        let plan: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {query}"))
            .bind(Uuid::new_v4())
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        assert!(plan.join("\n").contains(index), "{index}: {plan:?}");
    }
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_summary_projection_all_kinds_and_unknown_null(pool: PgPool) {
    receiver(&pool, true).await;
    let rule:Uuid=sqlx::query_scalar("INSERT INTO alert_rules(id,scope,kind,name,spike_factor_percent,min_spend_microusd,notify_platform_admins) VALUES(gen_random_uuid(),'installation','spend_spike','projection',200,1,true) RETURNING id").fetch_one(&pool).await.unwrap();
    for (kind, summary) in [
        (
            "budget_threshold",
            "A shared workspace budget threshold was reached",
        ),
        (
            "spend_threshold",
            "An installation spend threshold was reached",
        ),
        ("spend_spike", "A spend spike was detected"),
        ("error_rate", "An error rate threshold was reached"),
        ("provider_failing", "An upstream connection is failing"),
        ("batch_failed", "A batch failed"),
        ("batch_stalled", "A batch stalled"),
        (
            "scim_last_admin",
            "SCIM tried to remove the last Platform Admin",
        ),
        (
            "partitions_missing",
            "Future history partitions are missing",
        ),
        (
            "history_orphans",
            "History parent verification found orphan rows",
        ),
    ] {
        let result: Option<String> = sqlx::query_scalar("SELECT alert_webhook_summary($1)")
            .bind(kind)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(result.as_deref(), Some(summary));
        let builtin = ["scim_last_admin", "partitions_missing", "history_orphans"].contains(&kind);
        let event:Uuid=sqlx::query_scalar("INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),$1,$2,$3,gen_random_uuid()::text,1,'warning','PRIVATE-CANARY') RETURNING id")
            .bind((!builtin).then_some(rule)).bind(builtin.then_some(kind)).bind(kind).fetch_one(&pool).await.unwrap();
        sqlx::query(DELIVERY)
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        let payload: Vec<u8> =
            sqlx::query_scalar("SELECT payload FROM alert_webhook_outbox WHERE event_id=$1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(payload["event"]["summary"], summary);
        assert!(!payload.to_string().contains("PRIVATE-CANARY"));
    }
    let unknown: Option<String> = sqlx::query_scalar("SELECT alert_webhook_summary('future_kind')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(unknown, None);
    // Simulate a future evaluator kind without adding it to this projection.
    sqlx::query("ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check")
        .execute(&pool)
        .await
        .unwrap();
    let event:Uuid=sqlx::query_scalar("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),$1,'future_kind',gen_random_uuid()::text,1,'warning','PRIVATE-CANARY') RETURNING id").bind(rule).fetch_one(&pool).await.unwrap();
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
