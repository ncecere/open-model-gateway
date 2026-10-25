#![cfg(feature = "integration-tests")]
use open_model_gateway::store::{MIGRATOR, Store};
use sqlx::PgPool;
#[sqlx::test(migrations = false)]
async fn scheduled_reports_0065_ordered_prefix_and_no_replay(pool: PgPool) {
    assert_eq!(
        MIGRATOR
            .iter()
            .find(|m| m.version == 65)
            .unwrap()
            .description,
        "report delivery"
    );
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|m| m.version < 65)
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
    assert_eq!(
        sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT version,checksum FROM _sqlx_migrations WHERE version<65 ORDER BY version"
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        prefix
    );
    assert!(store.readiness().await.schema);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_deliveries WHERE report_run_id IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=65")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store.readiness().await.schema);
}
