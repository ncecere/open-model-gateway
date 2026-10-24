#![cfg(feature = "integration-tests")]
use open_model_gateway::store::{MIGRATOR, Store};
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = false)]
async fn scheduled_reports_explicit_0060_upgrade_and_readiness(pool: PgPool) {
    let report = MIGRATOR
        .iter()
        .find(|m| m.version == 60)
        .expect("0060 report schedules, not reserved 0058/0059");
    assert_eq!(report.description, "report schedules");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|m| m.version < 60)
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
            "SELECT version,checksum FROM _sqlx_migrations WHERE version<60 ORDER BY version"
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
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=60")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !store.readiness().await.schema,
        "checksum mismatch refuses readiness"
    );
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
