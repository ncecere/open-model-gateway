#![cfg(feature = "integration-tests")]
use open_model_gateway::store::{MIGRATOR, Store};
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = false)]
async fn scheduled_reports_explicit_0061_upgrade_and_readiness(pool: PgPool) {
    let report = MIGRATOR
        .iter()
        .find(|m| m.version == 61)
        .expect("0061 report schedules, not reserved 0058/0059/0060");
    assert_eq!(report.description, "report schedules");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|m| m.version < 61)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    let store = Store::new(pool.clone());
    assert!(!store.readiness().await.schema);
    store.migrate_enterprise().await.unwrap();
    store.preflight_enterprise().await.unwrap();
    assert!(store.readiness().await.schema);
    assert_eq!(
        sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT version,checksum FROM _sqlx_migrations WHERE version<61 ORDER BY version"
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM report_schedules")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "no backfill or historical replay"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT epoch FROM work_leases WHERE name='report_scheduler'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    store.migrate_enterprise().await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=61")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !store.readiness().await.schema,
        "checksum mismatch refuses readiness"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_review_bookkeeping_preserves_configuration_revision(pool: PgPool) {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'report-bookkeeping@example.test')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,created_by,cadence,timezone,delivery_time) VALUES('installation',$1,'weekly','UTC','09:00') RETURNING id")
        .bind(owner).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,1,$2)")
        .bind(id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let revision = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT configuration_revision FROM report_schedules WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(revision().await, 1);
    // Migrator credentials only: slice A deliberately grants runtime no cursor writes.
    sqlx::query("UPDATE report_schedules SET next_run_at=clock_timestamp()+interval '7 days',last_period_end=clock_timestamp() WHERE id=$1")
        .bind(id).execute(&pool).await.unwrap();
    assert_eq!(
        revision().await,
        1,
        "a worker advancing bookkeeping must not invalidate its configuration revision"
    );
    let advanced: bool =
        sqlx::query_scalar("SELECT next_run_at>last_period_end FROM report_schedules WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(advanced);
    sqlx::query("UPDATE report_schedules SET cadence='monthly' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        revision().await,
        2,
        "a real configuration edit advances revision"
    );
    sqlx::query("UPDATE report_schedules SET state=state,next_run_at=next_run_at+interval '1 day' WHERE id=$1")
        .bind(id).execute(&pool).await.unwrap();
    assert_eq!(revision().await, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_review_indexes_match_scope_id_continuation(pool: PgPool) {
    for (index, expected) in [
        ("report_schedules_platform", vec!["id"]),
        ("report_schedules_workspace", vec!["workspace_id", "id"]),
    ] {
        let columns:Vec<String> = sqlx::query_scalar("SELECT array_agg(a.attname::text ORDER BY k.ordinality) FROM pg_index i CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum,ordinality) JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.attnum WHERE i.indexrelid=to_regclass($1)")
            .bind(index).fetch_one(&pool).await.unwrap();
        assert_eq!(columns, expected, "{index}");
        let predicate: String = sqlx::query_scalar(
            "SELECT pg_get_expr(indpred,indrelid) FROM pg_index WHERE indexrelid=to_regclass($1)",
        )
        .bind(index)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(predicate.contains("deleted_at IS NULL"));
        if index == "report_schedules_platform" {
            assert!(predicate.contains("workspace_id IS NULL"));
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_schema_scope_guards_cap_and_retained_identity(pool: PgPool) {
    let owner = Uuid::new_v4();
    let ws = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'report-schema@example.test')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Private','personal',$2)",
    )
    .bind(ws)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    let insert = "INSERT INTO report_schedules(scope_kind,workspace_id,created_by,cadence,timezone,delivery_time) VALUES($1,$2,$3,'weekly','UTC','09:00') RETURNING id";
    for (kind, workspace) in [
        ("workspace", Some(ws)),
        ("personal", None),
        ("installation", Some(ws)),
        ("cost_center", None),
    ] {
        assert!(
            sqlx::query(insert)
                .bind(kind)
                .bind(workspace)
                .bind(owner)
                .execute(&pool)
                .await
                .is_err(),
            "invalid scope shape: {kind}"
        );
    }
    let id: Uuid = sqlx::query_scalar(insert)
        .bind("personal")
        .bind(ws)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,1,$2)")
        .bind(id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    for statement in [
        format!("UPDATE report_schedules SET workspace_id=NULL WHERE id='{id}'"),
        format!("UPDATE report_schedules SET created_by=gen_random_uuid() WHERE id='{id}'"),
        format!("UPDATE report_schedules SET configuration_revision=0 WHERE id='{id}'"),
        format!("UPDATE report_schedules SET cadence='daily' WHERE id='{id}'"),
        format!("DELETE FROM report_schedules WHERE id='{id}'"),
        "TRUNCATE report_schedules CASCADE".into(),
        format!(
            "INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES('{id}',51,'{owner}')"
        ),
        format!(
            "INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES('{id}',2,'{owner}')"
        ),
        format!(
            "INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES('{id}',2,gen_random_uuid())"
        ),
    ] {
        assert!(
            sqlx::query(&statement).execute(&pool).await.is_err(),
            "{statement}"
        );
    }
    sqlx::query(
        "UPDATE report_schedules SET deleted_at=clock_timestamp(),state='paused' WHERE id=$1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        sqlx::query("UPDATE report_schedules SET deleted_at=NULL WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err(),
        "deletion is final"
    );
}
