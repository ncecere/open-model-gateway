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
