//! Disallowed SMTP relays fail before connecting, but still record the test.
use super::*;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn email_test_disallowed_host_records_failure(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin = browser(&pool, &f.admin).await;
    let g = Gateway::start(f.s, &pool, crate::filestore::FileStoreRuntime::memory()).await;
    let path = format!("{SETTINGS}/email");
    let saved = g
        .send(
            &admin,
            "PUT",
            &path,
            json!({"host":"smtp.example.test","port":587,"tls":"starttls","from_address":"gateway@example.test"}),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "save status");
    assert!(value(&saved)["host_allowed"] == false, "/host_allowed");
    let tested = g
        .send(&admin, "POST", &format!("{path}/test"), json!({}))
        .await;
    assert_eq!(tested.status, StatusCode::OK, "test status");
    let outcome = value(&tested);
    assert!(outcome["ok"] == false, "/ok");
    assert!(outcome["error"] == "host_not_allowed", "/error");
    let read = g.send(&admin, "GET", &path, Value::Null).await;
    assert_eq!(read.status, StatusCode::OK, "read status");
    let stored = value(&read);
    assert!(stored["last_test"]["ok"] == false, "/last_test/ok");
    assert!(
        stored["last_test"]["error"] == "host_not_allowed",
        "/last_test/error"
    );
    assert!(stored["last_test"]["at"].is_string(), "/last_test/at");
    let recorded: bool = sqlx::query_scalar(
        "SELECT smtp_last_test_at IS NOT NULL AND smtp_last_test_ok = false AND smtp_last_test_error = 'host_not_allowed' FROM installation_settings",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(recorded, "persisted test outcome");
}

#[sqlx::test(migrations = false)]
async fn smtp_test_error_upgrade_preserves_outcomes_and_closed_vocabulary(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version < 68).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE installation_settings SET smtp_last_test_at = now(), smtp_last_test_ok = false, smtp_last_test_error = 'connection'")
        .execute(&pool).await.unwrap();
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM installation_settings s")
        .fetch_one(&pool)
        .await
        .unwrap();
    all.run(&pool).await.unwrap();
    let after: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM installation_settings s")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(before == after, "upgrade preserves settings and outcome");
    for category in [
        None,
        Some("credential"),
        Some("address"),
        Some("connection"),
        Some("tls"),
        Some("authentication"),
        Some("rejected"),
        Some("timeout"),
        Some("interrupted"),
        Some("host_not_allowed"),
    ] {
        assert!(
            sqlx::query("UPDATE installation_settings SET smtp_last_test_error = $1")
                .bind(category)
                .execute(&pool)
                .await
                .is_ok(),
            "allowed delivery category"
        );
    }
    let error = sqlx::query("UPDATE installation_settings SET smtp_last_test_error = 'unknown'")
        .execute(&pool)
        .await
        .expect_err("unknown category must fail");
    assert!(
        error
            .as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514")),
        "unknown category CHECK violation"
    );
}
