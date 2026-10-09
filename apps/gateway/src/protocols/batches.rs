//! OpenAI-compatible Batch API (`crate::jobs`, docs/batches.md):
//! - `POST /v1/batches` `{input_file_id, endpoint, completion_window:"24h"
//!   (or 48h, 72h, 168h: gateway-run), metadata?}` where `input_file_id` is a gateway file (`purpose=batch`,
//!   `/v1/files`) and `endpoint` is `/v1/chat/completions`, `/v1/responses`,
//!   `/v1/embeddings` or `/v1/messages`. An invalid file is rejected with a
//!   line-numbered report (`errors.data[].line`).
//! - `GET /v1/batches` (this workspace's batches, newest first),
//!   `GET /v1/batches/{id}`, `POST /v1/batches/{id}/cancel`.
use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::videos::{ListQuery, job_error, list_page, page_params};
use crate::{
    auth::Principal,
    filestore::FileStoreRuntime,
    http::RequestId,
    inference::{Engine, error::InferenceError},
    jobs::{
        Jobs,
        batch::{CreateBatch, CreateError},
        types::{BatchEndpoint, completion_window_hours},
    },
    store::Store,
};

fn jobs(store: Store, engine: &Engine, files: Option<Extension<FileStoreRuntime>>) -> Jobs {
    Jobs::new(store, engine).with_files(files.map(|Extension(f)| f))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    input_file_id: String,
    endpoint: String,
    completion_window: String,
    metadata: Option<Map<String, Value>>,
    output_expires_after: Option<Value>,
}

/// 400 with the validation report (OpenAI `errors` list shape).
fn invalid_input(issues: &[crate::jobs::plan::Issue]) -> Response {
    let data: Vec<Value> = issues
        .iter()
        .map(|i| json!({"code": i.error.code(), "message": i.error.message(), "line": i.line, "param": null}))
        .collect();
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": "The input file has invalid lines; see errors.data (at most 20 are listed).",
                "type": "invalid_request_error",
                "code": "invalid_batch_input",
                "param": "input_file_id",
            },
            "errors": {"object": "list", "data": data},
        })),
    )
        .into_response()
}

pub async fn create(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    files: Option<Extension<FileStoreRuntime>>,
    input: Result<Json<CreateRequest>, JsonRejection>,
) -> Response {
    let request = match input {
        Ok(Json(r)) => r,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return job_error(InferenceError::InvalidRequest.into()),
    };
    if request.output_expires_after.is_some() {
        return job_error(InferenceError::Unsupported.into());
    }
    // 24h (default), 48h, 72h or 168h; longer windows run gateway-side.
    let Some(window_hours) = completion_window_hours(&request.completion_window) else {
        return job_error(InferenceError::InvalidRequest.into());
    };
    let Some(endpoint) = BatchEndpoint::parse(&request.endpoint) else {
        return job_error(InferenceError::Unsupported.into());
    };
    let jobs = jobs(store, &engine, files);
    match jobs
        .create_batch(
            principal,
            request_id.0,
            CreateBatch {
                input_file_id: request.input_file_id,
                endpoint,
                metadata: request.metadata,
                completion_window_hours: Some(window_hours),
            },
        )
        .await
    {
        Ok(job) => match jobs.batch_object(&job).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => job_error(e),
        },
        Err(CreateError::Invalid(issues)) => invalid_input(&issues),
        Err(CreateError::Job(e)) => job_error(e),
    }
}

pub async fn retrieve(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    files: Option<Extension<FileStoreRuntime>>,
    Path(id): Path<String>,
) -> Response {
    match jobs(store, &engine, files).get_batch(&principal, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => job_error(e),
    }
}

pub async fn cancel(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    files: Option<Extension<FileStoreRuntime>>,
    Path(id): Path<String>,
) -> Response {
    match jobs(store, &engine, files)
        .cancel_batch(&principal, &id)
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => job_error(e),
    }
}

pub async fn list(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    files: Option<Extension<FileStoreRuntime>>,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(q)) = query else {
        return job_error(InferenceError::InvalidRequest.into());
    };
    let (limit, ascending) = match page_params(&q) {
        Ok(p) => p,
        Err(e) => return job_error(e),
    };
    if ascending {
        // The Batch API lists newest first only.
        return job_error(InferenceError::InvalidRequest.into());
    }
    match jobs(store, &engine, files)
        .list_batches(&principal, q.after.as_deref(), limit + 1)
        .await
    {
        Ok(rows) => Json(list_page(rows, limit)).into_response(),
        Err(e) => job_error(e),
    }
}
