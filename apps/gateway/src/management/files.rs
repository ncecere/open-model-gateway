//! Workspace › Files: the dashboard view of the gateway Files API store
//! (docs/files-api.md). Same files as `GET /v1/files` for the workspace.
//!
//! Visibility follows request details: workspace admins (and the personal
//! owner) see every file; other members see files they uploaded themselves
//! (from the dashboard or with their own keys). Platform roles without
//! membership see nothing. Uploads use the same pipeline as `POST /v1/files`;
//! contents never reach audit or logs.
use super::*;
use crate::filestore::{
    FileStoreRuntime, StoredFile,
    files::{API_PURPOSES, FileList, FileStorage, parse_public_id, public_id},
    upload::{self, UploadError, Uploader},
};
use axum::{body::Body, http::HeaderMap, routing::delete};

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/workspaces/{ws}/files", get(list))
        .route("/api/v1/workspaces/{ws}/files/{id}", delete(remove))
        .route("/api/v1/workspaces/{ws}/files/{id}/content", get(content))
}

/// The upload route carries its own body cap (`GATEWAY_FILES_MAX_BYTES`), so
/// it is mounted outside the shared 2 MiB management limit.
pub(super) fn upload_routes() -> Router<Store> {
    let bytes = upload::limits().body_bytes();
    Router::new().route(
        "/api/v1/workspaces/{ws}/files/upload",
        post(upload_file)
            .layer::<_, std::convert::Infallible>(axum::extract::DefaultBodyLimit::max(bytes))
            .layer(tower_http::limit::RequestBodyLimitLayer::new(bytes)),
    )
}

fn storage(s: &Store, ext: Option<Extension<FileStoreRuntime>>) -> FileStorage {
    FileStorage::new(s.clone(), ext.map(|Extension(r)| r).unwrap_or_default())
}

fn render(f: &StoredFile, me: Uuid) -> Value {
    json!({
        "id": public_id(f.id),
        "filename": f.filename.clone().unwrap_or_else(|| "file".into()),
        "purpose": f.api_purpose,
        "bytes": f.size_bytes,
        "content_type": f.content_type,
        "created_at": f.created_at,
        "expires_at": f.expires_at,
        "status": "processed",
        "mine": f.created_by_user_id == Some(me),
        "via_key": f.created_by_api_key_id.is_some(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    purpose: Option<String>,
    search: Option<String>,
    limit: Option<i64>,
    after: Option<String>,
}

const STORE_UNAVAILABLE: &str = "File storage unavailable";

fn store_error() -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, STORE_UNAVAILABLE)
}

async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
    Path(ws): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(100);
    if !(1..=200).contains(&limit)
        || q.purpose
            .as_deref()
            .is_some_and(|p| !API_PURPOSES.contains(&p))
        || q.search.as_deref().is_some_and(|t| t.chars().count() > 120)
    {
        return Err(invalid());
    }
    let after = q
        .after
        .as_deref()
        .map(|a| parse_public_id(a).ok_or_else(invalid))
        .transpose()?;
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let usage = crate::filestore::files::workspace_storage(&mut *tx, ws)
        .await
        .map_err(|_| store_error())?;
    tx.commit().await?;
    let files = storage(&s, ext);
    let policy = files.policy().await.map_err(|_| store_error())?;
    let configured = files.runtime().store().is_some();
    let query = FileList {
        purpose: q.purpose,
        after,
        limit,
        ascending: false,
        created_by_user_id: (!a.view_all_activity).then_some(u.user_id),
        search: q.search.filter(|t| !t.trim().is_empty()),
    };
    let (page, more) = files.list(ws, &query).await.map_err(|_| store_error())?;
    let data: Vec<Value> = page.iter().map(|f| render(f, u.user_id)).collect();
    let group = |g| configured && policy.enabled(g);
    Ok(Json(json!({
        "data": data,
        "has_more": more,
        "scope": if a.view_all_activity { "workspace" } else { "own" },
        "storage": {
            "quota_bytes": usage.quota_bytes,
            "used_bytes": a.view_all_activity.then_some(usage.used_bytes),
        },
        "store": {
            "configured": configured,
            "batch": group(crate::filestore::PurposeGroup::Batch),
            "user_files": group(crate::filestore::PurposeGroup::UserFiles),
        },
        "max_bytes": upload::limits().max_bytes,
    })))
}

/// A file this caller may see in the workspace.
async fn visible(
    s: &Store,
    u: &BrowserPrincipal,
    files: &FileStorage,
    ws: Uuid,
    id: &str,
) -> Result<(StoredFile, resources::WorkspaceAccess), ApiError> {
    let id = parse_public_id(id).ok_or_else(missing)?;
    let (tx, a) = resources::workspace_tx(s, u, ws).await?;
    resources::detail_access(&a)?;
    tx.commit().await?;
    let file = files
        .get(id, Some(ws))
        .await
        .map_err(|_| store_error())?
        .filter(|f| f.api_purpose.is_some())
        .filter(|f| a.view_all_activity || f.created_by_user_id == Some(u.user_id))
        .ok_or_else(missing)?;
    Ok((file, a))
}

async fn content(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
    Path((ws, id)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    let files = storage(&s, ext);
    let (file, _) = visible(&s, &u, &files, ws, &id).await?;
    let (file, stream) = files.open(file.id, Some(ws)).await.map_err(|e| match e {
        crate::filestore::FileError::NotFound => missing(),
        _ => store_error(),
    })?;
    Ok(crate::protocols::files::content_response(&file, stream))
}

async fn remove(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
    Path((ws, id)): Path<(Uuid, String)>,
) -> ApiResult {
    let files = storage(&s, ext);
    let (file, a) = visible(&s, &u, &files, ws, &id).await?;
    // Admins delete any workspace file; members only their own.
    if !a.admin && file.created_by_user_id != Some(u.user_id) {
        return Err(denied());
    }
    if !files
        .delete(file.id, Some(ws))
        .await
        .map_err(|_| store_error())?
    {
        return Err(missing());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "file.deleted",
        "file",
        Some(file.id),
        json!({"kind": file.api_purpose}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

async fn upload_file(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
    Path(ws): Path<Uuid>,
    headers: HeaderMap,
    request: Body,
) -> Result<Response, ApiError> {
    let (tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    tx.commit().await?;
    let files = storage(&s, ext);
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let who = Uploader {
        workspace_id: ws,
        api_key_id: None,
        user_id: Some(u.user_id),
    };
    let file = match upload::receive(
        &files,
        who,
        content_type,
        request.into_data_stream(),
        upload::limits().max_bytes,
    )
    .await
    {
        Ok(f) => f,
        // Same machine-readable codes as the Files API.
        Err(e) => return Ok(dashboard_upload_error(e)),
    };
    let mut tx = resources::installation_tx(&s).await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "file.uploaded",
        "file",
        Some(file.id),
        json!({"kind": file.api_purpose}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(render(&file, u.user_id)).into_response())
}

/// Management error envelope (`error.code` is the HTTP status, `error.reason`
/// the Files API code).
fn dashboard_upload_error(e: UploadError) -> Response {
    let (status, reason, message) = match e {
        UploadError::StoreOff => (
            StatusCode::SERVICE_UNAVAILABLE,
            "file_storage_not_configured",
            "File storage is not configured on the server",
        ),
        UploadError::PurposeOff => (
            StatusCode::FORBIDDEN,
            "file_purpose_disabled",
            "Files for this purpose are turned off in Settings",
        ),
        UploadError::Invalid { message, .. } => {
            (StatusCode::BAD_REQUEST, "invalid_request_error", message)
        }
        UploadError::UnsupportedPurpose => (
            StatusCode::BAD_REQUEST,
            "unsupported_capability",
            "This purpose is not supported",
        ),
        UploadError::TooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "The file exceeds the maximum upload size",
        ),
        UploadError::QuotaExceeded => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "storage_quota_exceeded",
            "The workspace storage quota would be exceeded",
        ),
        UploadError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "file_storage_unavailable",
            STORE_UNAVAILABLE,
        ),
    };
    (
        status,
        Json(
            json!({"error":{"code":status.as_u16().to_string(),"message":message,"reason":reason}}),
        ),
    )
        .into_response()
}
