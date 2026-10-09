//! Admin › Settings › Storage: authority, defaults, the backend/health gate on
//! customer-content toggles, validation, auditing and test rate limits.
use super::*;
use crate::filestore::{
    BackendKind, ByteStream, FileStore, FileStoreError, FileStoreRuntime, ObjectInfo, ObjectKey,
    PutMeta, StoreHealth, StoredObject,
};
use std::sync::Arc;

const STORAGE: &str = "/api/v1/platform/settings/storage";
const TEST: &str = "/api/v1/platform/settings/storage/test";

async fn call_rt(
    s: &Store,
    u: &BrowserPrincipal,
    rt: Option<&FileStoreRuntime>,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut router = routes().layer(Extension(u.clone()));
    if let Some(rt) = rt {
        router = router.layer(Extension(rt.clone()));
    }
    let response = router
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn input(batch: bool, days: i32) -> Value {
    json!({
        "batch": {"enabled": batch, "retention_days": days},
        "video": {"enabled": false, "retention_days": 7},
        "user_files": {"enabled": false, "retention_days": 30},
        "export": {"retention_days": 1},
    })
}

fn group<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["group"] == name)
        .unwrap()
}

/// A store whose every operation fails, like an unreachable bucket.
struct Broken;
#[async_trait::async_trait]
impl FileStore for Broken {
    async fn put(
        &self,
        _: &ObjectKey,
        _: ByteStream,
        _: PutMeta,
    ) -> Result<StoredObject, FileStoreError> {
        Err(FileStoreError::Unavailable)
    }
    async fn get(&self, _: &ObjectKey) -> Result<ByteStream, FileStoreError> {
        Err(FileStoreError::Unavailable)
    }
    async fn delete(&self, _: &ObjectKey) -> Result<(), FileStoreError> {
        Err(FileStoreError::Unavailable)
    }
    async fn head(&self, _: &ObjectKey) -> Result<Option<ObjectInfo>, FileStoreError> {
        Err(FileStoreError::Unavailable)
    }
    async fn health(&self) -> Result<StoreHealth, FileStoreError> {
        Err(FileStoreError::Unavailable)
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn storage_settings_are_admin_write_auditor_read_with_safe_defaults(pool: PgPool) {
    let f = fixture(&pool).await;
    for user in [&f.admin, &f.auditor] {
        let (status, v) = call_rt(&f.s, user, None, "GET", STORAGE, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["backend"], "off");
        assert_eq!(v["location"], Value::Null);
        assert_eq!(v["encryption"], Value::Null);
        assert_eq!(group(&v, "batch")["enabled"], false);
        assert_eq!(group(&v, "batch")["retention_days"], 7);
        assert_eq!(group(&v, "video")["retention_days"], 7);
        assert_eq!(group(&v, "user_files")["retention_days"], 30);
        assert_eq!(group(&v, "export")["retention_days"], 1);
        assert_eq!(group(&v, "export")["toggle"], false);
        assert_eq!(group(&v, "branding")["retention_days"], Value::Null);
        assert_eq!(
            group(&v, "batch")["purposes"],
            json!(["batch_input", "batch_output"])
        );
    }
    for user in [&f.owner, &f.member] {
        assert_eq!(
            call_rt(&f.s, user, None, "GET", STORAGE, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let rt = FileStoreRuntime::memory();
    assert_eq!(
        call_rt(&f.s, &f.auditor, Some(&rt), "PUT", STORAGE, input(true, 7))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call_rt(&f.s, &f.auditor, Some(&rt), "POST", TEST, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // Without a backend, customer-content storage cannot be turned on or tested.
    let (status, err) = call_rt(&f.s, &f.admin, None, "PUT", STORAGE, input(true, 7)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"]["reason"], "file_storage_not_configured");
    let (status, err) = call_rt(&f.s, &f.admin, None, "POST", TEST, json!({})).await;
    assert_eq!(
        (status, err["error"]["reason"].as_str()),
        (StatusCode::CONFLICT, Some("file_storage_not_configured"))
    );
    // Retention can still be prepared while off.
    let (status, v) = call_rt(&f.s, &f.admin, None, "PUT", STORAGE, input(false, 14)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(group(&v, "batch")["retention_days"], 14);
    for bad in [input(false, 0), input(false, 366)] {
        assert_eq!(
            call_rt(&f.s, &f.admin, None, "PUT", STORAGE, bad).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut extra = input(false, 7);
    extra["branding"] = json!({"retention_days": 1});
    assert_eq!(
        call_rt(&f.s, &f.admin, None, "PUT", STORAGE, extra).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn enabling_requires_a_healthy_store_and_is_audited(pool: PgPool) {
    let f = fixture(&pool).await;
    let broken = FileStoreRuntime::from_store(Arc::new(Broken), BackendKind::S3, vec!["k1".into()]);
    let (status, err) = call_rt(
        &f.s,
        &f.admin,
        Some(&broken),
        "PUT",
        STORAGE,
        input(true, 7),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"]["reason"], "file_storage_unhealthy");
    let (_, v) = call_rt(&f.s, &f.admin, Some(&broken), "GET", STORAGE, json!({})).await;
    assert_eq!(group(&v, "batch")["enabled"], false);
    assert_eq!(v["health"]["ok"], false);
    assert_eq!(v["health"]["error"], "unavailable");
    assert_eq!(v["health"]["current"], true);
    assert_eq!(v["backend"], "s3");

    let rt = FileStoreRuntime::memory();
    let (status, test) = call_rt(&f.s, &f.admin, Some(&rt), "POST", TEST, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{test}");
    assert_eq!(
        (test["ok"].as_bool(), test["backend"].as_str()),
        (Some(true), Some("memory"))
    );
    let (status, v) = call_rt(&f.s, &f.admin, Some(&rt), "PUT", STORAGE, input(true, 3)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let batch = group(&v, "batch");
    assert_eq!(
        (batch["enabled"].as_bool(), batch["active"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(batch["retention_days"], 3);
    assert_eq!(v["encryption"]["key_id"], "memory");
    assert_eq!(v["health"]["ok"], true);
    // The health result belongs to that configuration only.
    let (_, other) = call_rt(&f.s, &f.auditor, Some(&broken), "GET", STORAGE, json!({})).await;
    assert_eq!(other["health"]["current"], false);
    // Already on: saving again needs no probe, even if the store is now failing.
    assert_eq!(
        call_rt(
            &f.s,
            &f.admin,
            Some(&broken),
            "PUT",
            STORAGE,
            input(true, 5)
        )
        .await
        .0,
        StatusCode::OK
    );
    let audits: Vec<(String, Value)> = sqlx::query_as("SELECT action,metadata FROM audit_events WHERE action LIKE 'settings.storage%' ORDER BY created_at")
        .fetch_all(&pool)
        .await
        .unwrap();
    let actions: Vec<&str> = audits.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "settings.storage_test",
            "settings.storage_updated",
            "settings.storage_updated"
        ]
    );
    assert_eq!(audits[1].1["count"], 1);
    assert_eq!(audits[1].1["mode"], "memory");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn storage_tests_are_rate_limited(pool: PgPool) {
    let f = fixture(&pool).await;
    let rt = FileStoreRuntime::memory();
    for _ in 0..5 {
        assert_eq!(
            call_rt(&f.s, &f.admin, Some(&rt), "POST", TEST, json!({}))
                .await
                .0,
            StatusCode::OK
        );
    }
    let (status, err) = call_rt(&f.s, &f.admin, Some(&rt), "POST", TEST, json!({})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(err["error"]["reason"], "storage_test_rate_limited");
}
