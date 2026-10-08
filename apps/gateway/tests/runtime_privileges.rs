#![cfg(feature = "integration-tests")]
//! Explicitly opt in only on the dedicated disposable PostgreSQL test cluster.
use sqlx::PgPool;
#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "role/maintenance ACL probe: run only on a disposable test PostgreSQL cluster"]
async fn enterprise_runtime_allowlist_and_rollback_probes(pool: PgPool) {
    let mut connection = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(72419505)")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; END IF; END $$; REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime; REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime;")
        .execute(&mut *connection).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let database = database.replace('"', "\"\"");
    sqlx::raw_sql(&format!("REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE \"{database}\" TO gateway_runtime"))
        .execute(&mut *connection).await.unwrap();
    sqlx::raw_sql(include_str!("../../../deploy/staging/runtime-grants.sql"))
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../deploy/staging/verify-privileges.sql"
    ))
    .execute(&mut *connection)
    .await
    .unwrap();
    for table in [
        "users",
        "api_keys",
        "inference_executions",
        "monetary_ledger",
        "audit_events",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&mut *connection)
                .await
                .unwrap(),
            0,
            "rollback-only probe persisted {table}"
        );
    }
    sqlx::query("SELECT pg_advisory_unlock(72419505)")
        .execute(&mut *connection)
        .await
        .unwrap();
}
