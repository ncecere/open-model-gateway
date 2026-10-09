//! OpenAI-compatible Files API on the gateway's encrypted file store
//! (docs/files-api.md). Inference-key auth; the workspace comes from the key.
//!
//! - `POST /v1/files` multipart: `purpose` (and optional
//!   `expires_after[anchor]`/`expires_after[seconds]`) before `file`; streamed
//!   into the store. `batch` → `batch_input`; `user_data`, `vision`,
//!   `assistants`, `evals` → `user_file`.
//! - `GET /v1/files` (`purpose`, `limit`, `after`, `order`), `GET /v1/files/{id}`,
//!   `GET /v1/files/{id}/content` (decrypted stream), `DELETE /v1/files/{id}`.
//!
//! Ids are gateway ids (`file-<32 hex>`); another workspace's id is 404. Batch
//! output files (written by the batch engine) are listed and downloadable.
use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::Principal,
    filestore::{
        FileStoreRuntime, StoredFile,
        files::{API_PURPOSES, FileError, FileList, FileStorage, parse_public_id, public_id},
        upload::{self, UploadError, Uploader},
    },
    store::Store,
};

/// OpenAI File object.
pub fn render(file: &StoredFile) -> Value {
    json!({
        "id": public_id(file.id),
        "object": "file",
        "bytes": file.size_bytes,
        "created_at": file.created_at.timestamp(),
        "expires_at": file.expires_at.map(|t| t.timestamp()),
        "filename": file.filename.clone().unwrap_or_else(|| "file".into()),
        "purpose": file.api_purpose,
        "status": "processed",
        "status_details": Value::Null,
    })
}

fn body(
    status: StatusCode,
    kind: &str,
    code: &str,
    message: &str,
    param: Option<&str>,
) -> Response {
    let mut response = (
        status,
        Json(json!({"error":{"message":message,"type":kind,"code":code,"param":param}})),
    )
        .into_response();
    if status != StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
    }
    response
}

pub(crate) fn upload_error(e: UploadError) -> Response {
    match e {
        UploadError::StoreOff => body(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "file_storage_not_configured",
            "File storage is not configured on this gateway",
            None,
        ),
        UploadError::PurposeOff => body(
            StatusCode::FORBIDDEN,
            "permission_error",
            "file_purpose_disabled",
            "Files for this purpose are turned off by the platform administrator",
            Some("purpose"),
        ),
        UploadError::Invalid { message, param } => body(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request_error",
            message,
            param,
        ),
        UploadError::UnsupportedPurpose => body(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_capability",
            "This gateway does not store files for this purpose",
            Some("purpose"),
        ),
        UploadError::TooLarge => body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "file_too_large",
            "The file exceeds the maximum upload size",
            Some("file"),
        ),
        UploadError::QuotaExceeded => body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "insufficient_quota",
            "storage_quota_exceeded",
            "The workspace storage quota would be exceeded; delete files or ask for a larger quota",
            Some("file"),
        ),
        UploadError::Unavailable => body(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "file_storage_unavailable",
            "File storage is temporarily unavailable",
            None,
        ),
    }
}

fn not_found() -> Response {
    body(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "not_found",
        "No such file",
        Some("file_id"),
    )
}

fn file_error(e: FileError) -> Response {
    match e {
        FileError::NotFound => not_found(),
        FileError::Disabled => upload_error(UploadError::StoreOff),
        FileError::Invalid => upload_error(UploadError::Invalid {
            message: "Invalid request",
            param: None,
        }),
        _ => upload_error(UploadError::Unavailable),
    }
}

pub(crate) fn storage(store: Store, runtime: Option<Extension<FileStoreRuntime>>) -> FileStorage {
    FileStorage::new(
        store,
        runtime.map_or_else(FileStoreRuntime::off, |Extension(r)| r),
    )
}

pub async fn upload(
    State(store): State<Store>,
    runtime: Option<Extension<FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    request: Body,
) -> Response {
    let files = storage(store, runtime);
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let who = Uploader {
        workspace_id: principal.workspace_id,
        api_key_id: Some(principal.key_id),
        user_id: principal.user_id,
    };
    match upload::receive(
        &files,
        who,
        content_type,
        request.into_data_stream(),
        upload::limits().max_bytes,
    )
    .await
    {
        Ok(file) => Json(render(&file)).into_response(),
        Err(e) => upload_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    purpose: Option<String>,
    limit: Option<String>,
    after: Option<String>,
    order: Option<String>,
}

pub async fn list(
    State(store): State<Store>,
    runtime: Option<Extension<FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let bad = |message: &'static str, param: &'static str| {
        upload_error(UploadError::Invalid {
            message,
            param: Some(param),
        })
    };
    let Ok(Query(q)) = query else {
        return bad("Unknown query parameter", "query");
    };
    let limit = match q.limit.as_deref().map(str::parse::<i64>) {
        None => 10_000,
        Some(Ok(n)) if (1..=10_000).contains(&n) => n,
        _ => return bad("limit must be between 1 and 10000", "limit"),
    };
    let ascending = match q.order.as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        _ => return bad("order must be asc or desc", "order"),
    };
    if q.purpose
        .as_deref()
        .is_some_and(|p| !API_PURPOSES.contains(&p))
    {
        return bad("Unknown purpose", "purpose");
    }
    let after = match q.after.as_deref().map(parse_public_id) {
        None => None,
        Some(Some(id)) => Some(id),
        Some(None) => return bad("after must be a file id", "after"),
    };
    let files = storage(store, runtime);
    let query = FileList {
        purpose: q.purpose,
        after,
        limit,
        ascending,
        created_by_user_id: None,
        search: None,
    };
    match files.list(principal.workspace_id, &query).await {
        Ok((page, has_more)) => {
            let data: Vec<Value> = page.iter().map(render).collect();
            Json(json!({
                "object": "list",
                "data": data,
                "first_id": page.first().map(|f| public_id(f.id)),
                "last_id": page.last().map(|f| public_id(f.id)),
                "has_more": has_more,
            }))
            .into_response()
        }
        Err(e) => file_error(e),
    }
}

/// A Files API file of this workspace (other purposes are invisible here).
async fn find(
    files: &FileStorage,
    principal: &Principal,
    id: &str,
) -> Result<StoredFile, FileError> {
    let id = parse_public_id(id).ok_or(FileError::NotFound)?;
    files
        .get(id, Some(principal.workspace_id))
        .await?
        .filter(|f| f.api_purpose.is_some())
        .ok_or(FileError::NotFound)
}

pub async fn retrieve(
    State(store): State<Store>,
    runtime: Option<Extension<FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let files = storage(store, runtime);
    match find(&files, &principal, &id).await {
        Ok(f) => Json(render(&f)).into_response(),
        Err(e) => file_error(e),
    }
}

/// Decrypted contents as an opaque download. A stream error (integrity,
/// store failure) aborts the response; it never ends cleanly.
pub(crate) fn content_response(
    file: &StoredFile,
    stream: crate::filestore::ByteStream,
) -> Response {
    let body = Body::from_stream(stream.map(|item| item.map_err(std::io::Error::other)));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(v) = HeaderValue::from_str(&file.size_bytes.to_string()) {
        headers.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&content_disposition(
        file.filename.as_deref().unwrap_or("file"),
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `attachment` with an ASCII fallback and the RFC 5987 UTF-8 name.
pub(crate) fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

pub async fn content(
    State(store): State<Store>,
    runtime: Option<Extension<FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let files = storage(store, runtime);
    let file = match find(&files, &principal, &id).await {
        Ok(f) => f,
        Err(e) => return file_error(e),
    };
    match files.open(file.id, Some(principal.workspace_id)).await {
        Ok((file, stream)) => content_response(&file, stream),
        Err(e) => file_error(e),
    }
}

pub async fn delete(
    State(store): State<Store>,
    runtime: Option<Extension<FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let files = storage(store, runtime);
    let file = match find(&files, &principal, &id).await {
        Ok(f) => f,
        Err(e) => return file_error(e),
    };
    match files.delete(file.id, Some(principal.workspace_id)).await {
        Ok(true) => Json(json!({"id": public_id(file.id), "object": "file", "deleted": true}))
            .into_response(),
        Ok(false) => not_found(),
        Err(e) => file_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_is_safe() {
        assert_eq!(
            content_disposition("a b.jsonl"),
            "attachment; filename=\"a b.jsonl\"; filename*=UTF-8''a%20b.jsonl"
        );
        let v = content_disposition("é\"x\\.txt");
        assert!(v.starts_with("attachment; filename=\"__x_.txt\""));
        assert!(HeaderValue::from_str(&v).is_ok());
    }

    #[test]
    fn ids_round_trip_strictly() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(parse_public_id(&public_id(id)), Some(id));
        assert_eq!(parse_public_id(&public_id(id).to_uppercase()), None);
        assert_eq!(parse_public_id("file-123"), None);
        assert_eq!(parse_public_id(&format!("batch_{}", id.simple())), None);
    }
}
