//! Admin › Settings › Data & privacy › Storage: the configured file store
//! (read-only, from the server environment), its health, and per purpose-group
//! toggles and retention (`installation_settings`, migration 0019).
//!
//! Enabling a group that holds customer content requires a configured backend
//! and a passing health probe, run before the settings transaction (never under
//! the installation lock). Tests are rate-limited from the audit trail like
//! email tests. Admin writes, Auditor reads.
use super::*;
use crate::filestore::{FileStoreRuntime, PurposeGroup, files::StoragePolicy};

pub(crate) const STORAGE_OFF: &str = "File storage is not configured on the server";
pub(crate) const STORAGE_UNHEALTHY: &str = "The file store failed its health check";
pub(crate) const STORAGE_TEST_LIMIT: &str = "Too many storage tests; wait a minute and try again";
const RETENTION_DAYS: std::ops::RangeInclusive<i32> = 1..=365;
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/settings/storage",
            get(storage).put(update_storage),
        )
        .route("/api/v1/platform/settings/storage/test", post(test_storage))
}

fn runtime(ext: Option<Extension<FileStoreRuntime>>) -> FileStoreRuntime {
    ext.map(|Extension(r)| r).unwrap_or_default()
}

fn label(group: PurposeGroup) -> &'static str {
    match group {
        PurposeGroup::Batch => "Batch files",
        PurposeGroup::Video => "Video outputs",
        PurposeGroup::UserFiles => "User files",
        PurposeGroup::Export => "Exports",
        PurposeGroup::Branding => "Branding",
    }
}

type CheckRow = (
    Option<Value>,
    Option<bool>,
    Option<String>,
    Option<String>,
    Value,
);

async fn storage_json(
    tx: &mut Transaction<'_, Postgres>,
    rt: &FileStoreRuntime,
) -> Result<Value, ApiError> {
    let policy = StoragePolicy::load(&mut **tx).await?;
    let (checked_at, ok, error, target, updated_at): CheckRow = sqlx::query_as("SELECT to_jsonb(file_store_last_check_at),file_store_last_check_ok,file_store_last_check_error,file_store_last_check_target,to_jsonb(updated_at) FROM installation_settings WHERE singleton")
        .fetch_one(&mut **tx)
        .await?;
    let totals: Vec<(String, i64, i64)> = sqlx::query_as("SELECT purpose,count(*),coalesce(sum(size_bytes),0)::bigint FROM stored_files WHERE deleted_at IS NULL AND committed_at IS NOT NULL GROUP BY purpose")
        .fetch_all(&mut **tx)
        .await?;
    let configured = rt.store().is_some();
    let groups: Vec<Value> = PurposeGroup::ALL
        .into_iter()
        .map(|g| {
            let (mut objects, mut bytes) = (0i64, 0i64);
            for (purpose, n, b) in &totals {
                if g.purposes().iter().any(|p| p.as_str() == purpose) {
                    objects += n;
                    bytes += b;
                }
            }
            let toggle = g.holds_customer_content();
            json!({
                "group": g.as_str(),
                "label": label(g),
                "purposes": g.purposes().iter().map(|p| p.as_str()).collect::<Vec<_>>(),
                "holds_customer_content": toggle,
                "toggle": toggle,
                "enabled": policy.enabled(g),
                "active": configured && policy.enabled(g),
                "retention_days": policy.retention_days(g),
                "retention_editable": g.default_retention_days().is_some(),
                "default_retention_days": g.default_retention_days(),
                "minimum": RETENTION_DAYS.start(),
                "maximum": RETENTION_DAYS.end(),
                "objects": objects,
                "bytes": bytes,
            })
        })
        .collect();
    let health = checked_at.map(|at| {
        json!({
            "checked_at": at,
            "ok": ok,
            "error": error,
            "current": target.is_some() && target.as_deref() == rt.fingerprint(),
        })
    });
    Ok(json!({
        "backend": rt.backend_name(),
        "location": rt.location(),
        "encryption": rt.active_key_id().map(|id| json!({
            "key_id": id,
            "decrypt_only_keys": rt.decrypt_only_keys(),
        })),
        "health": health,
        "groups": groups,
        "updated_at": updated_at,
    }))
}

async fn storage(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
) -> ApiResult {
    let rt = runtime(ext);
    let mut tx = read_tx(&s, &u).await?;
    let v = storage_json(&mut tx, &rt).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// Probe the store and record the outcome against the current configuration.
async fn probe(s: &Store, rt: &FileStoreRuntime) -> Result<Value, ApiError> {
    let store = rt
        .store()
        .ok_or(ApiError(StatusCode::CONFLICT, STORAGE_OFF))?;
    let result = match tokio::time::timeout(PROBE_TIMEOUT, store.health()).await {
        Ok(r) => r,
        Err(_) => Err(crate::filestore::FileStoreError::Timeout),
    };
    let error = result.as_ref().err().map(|e| e.code());
    sqlx::query("UPDATE installation_settings SET file_store_last_check_at=now(),file_store_last_check_ok=$1,file_store_last_check_error=$2,file_store_last_check_target=$3 WHERE singleton")
        .bind(error.is_none())
        .bind(error)
        .bind(rt.fingerprint())
        .execute(&s.pool)
        .await?;
    Ok(json!({
        "ok": error.is_none(),
        "error": error,
        "backend": rt.backend_name(),
        "round_trip_ms": result.as_ref().ok().map(|h| h.round_trip.as_millis() as u64),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupInput {
    enabled: bool,
    retention_days: i32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionInput {
    retention_days: i32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageInput {
    batch: GroupInput,
    video: GroupInput,
    user_files: GroupInput,
    export: RetentionInput,
}

async fn update_storage(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
    Json(b): Json<StorageInput>,
) -> ApiResult {
    let rt = runtime(ext);
    for days in [
        b.batch.retention_days,
        b.video.retention_days,
        b.user_files.retention_days,
        b.export.retention_days,
    ] {
        if !RETENTION_DAYS.contains(&days) {
            return Err(invalid());
        }
    }
    // Authorize before any probe, then release the lock for network I/O.
    let mut tx = write_tx(&s, &u).await?;
    let current = StoragePolicy::load(&mut *tx).await?;
    tx.commit().await?;
    let turning_on = (b.batch.enabled && !current.batch_enabled)
        || (b.video.enabled && !current.video_enabled)
        || (b.user_files.enabled && !current.user_files_enabled);
    if turning_on {
        if rt.store().is_none() {
            return Err(ApiError(StatusCode::CONFLICT, STORAGE_OFF));
        }
        if probe(&s, &rt).await?["ok"] != true {
            return Err(ApiError(StatusCode::CONFLICT, STORAGE_UNHEALTHY));
        }
    }
    let mut tx = write_tx(&s, &u).await?;
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=$1,file_batch_retention_days=$2,file_video_enabled=$3,file_video_retention_days=$4,file_user_files_enabled=$5,file_user_files_retention_days=$6,file_export_retention_days=$7,updated_at=now(),updated_by=$8 WHERE singleton")
        .bind(b.batch.enabled)
        .bind(b.batch.retention_days)
        .bind(b.video.enabled)
        .bind(b.video.retention_days)
        .bind(b.user_files.enabled)
        .bind(b.user_files.retention_days)
        .bind(b.export.retention_days)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    let enabled = [b.batch.enabled, b.video.enabled, b.user_files.enabled]
        .into_iter()
        .filter(|e| *e)
        .count();
    audit(
        &mut tx,
        &u,
        None,
        "settings.storage_updated",
        "installation_settings",
        None,
        json!({"count": enabled, "mode": rt.backend_name()}),
    )
    .await?;
    let v = storage_json(&mut tx, &rt).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn test_storage(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
) -> ApiResult {
    let rt = runtime(ext);
    let mut tx = write_tx(&s, &u).await?;
    if rt.store().is_none() {
        return Err(ApiError(StatusCode::CONFLICT, STORAGE_OFF));
    }
    let (mine, all): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE actor_user_id=$1 AND created_at>now()-interval '1 minute'),count(*) FROM audit_events WHERE action='settings.storage_test' AND created_at>now()-interval '1 hour'")
        .bind(u.user_id)
        .fetch_one(&mut *tx)
        .await?;
    if mine >= 5 || all >= 60 {
        return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, STORAGE_TEST_LIMIT));
    }
    audit(
        &mut tx,
        &u,
        None,
        "settings.storage_test",
        "installation_settings",
        None,
        json!({"mode": rt.backend_name()}),
    )
    .await?;
    // Never hold the installation lock across network I/O.
    tx.commit().await?;
    Ok(Json(probe(&s, &rt).await?))
}
