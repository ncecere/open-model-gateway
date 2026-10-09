//! OpenAI-compatible Files (batch input/output only) and Batch API (async
//! jobs, see `crate::jobs`):
//! - `POST /v1/files` multipart `purpose=batch` then `file` (JSONL): streamed
//!   to the provider while every line is validated; never stored or logged.
//! - `GET /v1/files/{id}`, `GET /v1/files/{id}/content` (streamed through).
//! - `POST /v1/batches` `{input_file_id, endpoint:"/v1/chat/completions",
//!   completion_window:"24h", metadata?}`, `GET /v1/batches` (gateway
//!   records of this workspace), `GET /v1/batches/{id}`,
//!   `POST /v1/batches/{id}/cancel`.
use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::videos::{ListQuery, job_error, list_page, page_params, stream_response};
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, error::InferenceError},
    jobs::{
        Jobs,
        batch::{render_batch, render_file},
        types::{BATCH_COMPLETION_WINDOW, BATCH_ENDPOINT},
    },
    store::Store,
};

pub async fn upload(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return job_error(InferenceError::InvalidRequest.into());
    };
    let jobs = Jobs::new(store, &engine);
    match jobs
        .upload_batch_file(
            principal,
            request_id.0,
            &content_type,
            body.into_data_stream(),
        )
        .await
    {
        Ok(file) => Json(render_file(&file)).into_response(),
        Err(e) => job_error(e),
    }
}

pub async fn file(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match Jobs::new(store, &engine).get_file(&principal, &id).await {
        Ok(f) => Json(render_file(&f)).into_response(),
        Err(e) => job_error(e),
    }
}

pub async fn file_content(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match Jobs::new(store, &engine)
        .file_content(&principal, &id)
        .await
    {
        Ok(content) => stream_response(content),
        Err(e) => job_error(e),
    }
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

pub async fn create(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
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
    if request.completion_window != BATCH_COMPLETION_WINDOW {
        return job_error(InferenceError::InvalidRequest.into());
    }
    if request.endpoint != BATCH_ENDPOINT {
        return job_error(InferenceError::Unsupported.into());
    }
    let jobs = Jobs::new(store, &engine);
    match jobs
        .create_batch(
            principal,
            request_id.0,
            &request.input_file_id,
            request.metadata,
        )
        .await
    {
        Ok((job, upstream)) => match jobs.batch_files(&job).await {
            Ok(files) => Json(render_batch(&job, files, Some(&upstream))).into_response(),
            Err(e) => job_error(e),
        },
        Err(e) => job_error(e),
    }
}

pub async fn retrieve(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let jobs = Jobs::new(store, &engine);
    match jobs.get_batch(&principal, &id).await {
        Ok((job, upstream)) => match jobs.batch_files(&job).await {
            Ok(files) => Json(render_batch(&job, files, upstream.as_ref())).into_response(),
            Err(e) => job_error(e),
        },
        Err(e) => job_error(e),
    }
}

pub async fn cancel(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let jobs = Jobs::new(store, &engine);
    match jobs.cancel_batch(&principal, &id).await {
        Ok((job, upstream)) => match jobs.batch_files(&job).await {
            Ok(files) => Json(render_batch(&job, files, Some(&upstream))).into_response(),
            Err(e) => job_error(e),
        },
        Err(e) => job_error(e),
    }
}

pub async fn list(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
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
    match Jobs::new(store, &engine)
        .list_batches(&principal, q.after.as_deref(), limit + 1)
        .await
    {
        Ok(rows) => Json(list_page(
            rows.iter()
                .map(|(job, files)| render_batch(job, *files, None))
                .collect(),
            limit,
        ))
        .into_response(),
        Err(e) => job_error(e),
    }
}
