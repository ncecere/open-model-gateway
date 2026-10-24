#![cfg(feature = "integration-tests")]
use open_model_gateway::store::{MIGRATOR, Store};
use sqlx::PgPool;
#[sqlx::test(migrations = false)]
async fn scheduled_reports_0063_upgrade_preserves_ordered_prefix_and_refuses_partial_readiness(
    pool: PgPool,
) {
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 63)
        .expect("0063 reserved for report generation");
    assert_eq!(migration.description, "report generation");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|m| m.version < 63)
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
    let prefix: Vec<(i64, Vec<u8>)> =
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
            "SELECT version,checksum FROM _sqlx_migrations WHERE version<63 ORDER BY version"
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        prefix
    );
    for table in [
        "report_runs",
        "report_snapshots",
        "report_generation_claims",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "no backfill: {table}"
        );
    }
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=63")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store.readiness().await.schema);
}
