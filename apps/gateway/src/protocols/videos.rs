//! OpenAI-compatible Videos API (async jobs, see `crate::jobs`):
//! - `POST /v1/videos`: multipart (as the official SDKs send it) or JSON
//!   `{model, prompt, seconds?, size?}`. `seconds` (4|8|12, default 4) and
//!   `size` (default `720x1280`) are always sent explicitly upstream so the
//!   billed duration and resolution are known. `input_reference`, remix,
//!   edits, extensions and characters are not supported.
//! - `GET /v1/videos` (gateway records of this workspace, cursor `after`),
//!   `GET /v1/videos/{id}`, `GET /v1/videos/{id}/content?variant=`,
//!   `DELETE /v1/videos/{id}`.
//!
//! Prompts and media are never logged or stored.
use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::audio::multipart;
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, error::InferenceError},
    jobs::{
        JobError, Jobs,
        types::{JobKind, VIDEO_DEFAULT_SECONDS, VideoAsset, VideoRequest, VideoSize, client_id},
        video::render,
    },
    store::Store,
};

/// Job error envelope: unknown/foreign ids are 404, invalid job state 400.
pub(crate) fn job_error(e: JobError) -> Response {
    let body = |status: StatusCode, code: &str, message: &str| {
        (
            status,
            Json(json!({"error":{"message":message,"type":"invalid_request_error","code":code,"param":null}})),
        )
            .into_response()
    };
    match e {
        JobError::NotFound => body(StatusCode::NOT_FOUND, "not_found", "No such object"),
        JobError::Conflict(code, message) => body(StatusCode::BAD_REQUEST, code, message),
        JobError::Inference(e) => super::workload_error(e, StatusCode::BAD_REQUEST),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRequest {
    model: Option<String>,
    prompt: Option<String>,
    seconds: Option<Value>,
    size: Option<String>,
    input_reference: Option<Value>,
}

fn seconds(value: Option<&str>) -> Result<u32, InferenceError> {
    match value {
        None => Ok(VIDEO_DEFAULT_SECONDS),
        Some(s) => s.parse().map_err(|_| InferenceError::InvalidRequest),
    }
}
fn normalize(
    model: Option<String>,
    prompt: Option<String>,
    secs: Option<&str>,
    size: Option<&str>,
) -> Result<VideoRequest, InferenceError> {
    let request = VideoRequest {
        // The gateway has no implicit default model.
        model: model.ok_or(InferenceError::InvalidRequest)?,
        prompt: prompt.ok_or(InferenceError::InvalidRequest)?,
        seconds: seconds(secs)?,
        size: size
            .map(|s| VideoSize::parse(s).ok_or(InferenceError::InvalidRequest))
            .transpose()?
            .unwrap_or(VideoSize::DEFAULT),
    };
    request.validate()?;
    Ok(request)
}

fn parse(headers: &HeaderMap, body: &[u8]) -> Result<VideoRequest, InferenceError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or(InferenceError::InvalidRequest)?;
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media == "application/json" {
        let r: JsonRequest =
            serde_json::from_slice(body).map_err(|_| InferenceError::InvalidRequest)?;
        if r.input_reference.is_some() {
            return Err(InferenceError::Unsupported);
        }
        let secs = match &r.seconds {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) if n.is_u64() => Some(n.to_string()),
            Some(_) => return Err(InferenceError::InvalidRequest),
        };
        return normalize(r.model, r.prompt, secs.as_deref(), r.size.as_deref());
    }
    let boundary = multipart::boundary(content_type)?;
    let parts = multipart::parse(body, &boundary, 8)?;
    let mut fields: std::collections::BTreeMap<&str, String> = Default::default();
    for part in &parts {
        match part.name.as_str() {
            "input_reference" => return Err(InferenceError::Unsupported),
            name @ ("model" | "prompt" | "seconds" | "size") if part.filename.is_none() => {
                let text = std::str::from_utf8(part.data)
                    .map_err(|_| InferenceError::InvalidRequest)?
                    .to_owned();
                if fields.insert(name, text).is_some() {
                    return Err(InferenceError::InvalidRequest);
                }
            }
            _ => return Err(InferenceError::InvalidRequest),
        }
    }
    normalize(
        fields.remove("model"),
        fields.remove("prompt"),
        fields.get("seconds").map(String::as_str),
        fields.get("size").map(String::as_str),
    )
}

pub async fn create(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let body = match body {
        Ok(b) => b,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return job_error(InferenceError::InvalidRequest.into()),
    };
    let request = match parse(&headers, &body) {
        Ok(r) => r,
        Err(e) => return job_error(e.into()),
    };
    drop(body);
    let jobs = Jobs::new(store, &engine);
    match jobs.create_video(principal, request, request_id.0).await {
        Ok(job) => Json(render(&job)).into_response(),
        Err(e) => job_error(e),
    }
}

pub async fn retrieve(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match Jobs::new(store, &engine).get_video(&principal, &id).await {
        Ok(job) => Json(render(&job)).into_response(),
        Err(e) => job_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: Option<i64>,
    pub(crate) order: Option<String>,
}
/// `limit` 1..=100 (default 20), `order` asc|desc (default desc).
pub(crate) fn page_params(q: &ListQuery) -> Result<(i64, bool), JobError> {
    let limit = q.limit.unwrap_or(20);
    let ascending = match q.order.as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        Some(_) => return Err(InferenceError::InvalidRequest.into()),
    };
    if !(1..=100).contains(&limit) {
        return Err(InferenceError::InvalidRequest.into());
    }
    Ok((limit, ascending))
}
pub(crate) fn list_page(mut data: Vec<Value>, limit: i64) -> Value {
    let has_more = data.len() > limit as usize;
    data.truncate(limit as usize);
    let first = data.first().map(|v| v["id"].clone());
    let last = data.last().map(|v| v["id"].clone());
    json!({"object":"list","data":data,"first_id":first,"last_id":last,"has_more":has_more})
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
    match Jobs::new(store, &engine)
        .list_videos(&principal, q.after.as_deref(), limit + 1, ascending)
        .await
    {
        Ok(rows) => Json(list_page(rows.iter().map(render).collect(), limit)).into_response(),
        Err(e) => job_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQuery {
    variant: Option<String>,
}

pub(crate) fn stream_response(content: crate::jobs::types::ContentStream) -> Response {
    let mut response = Response::new(Body::from_stream(content.body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content.content_type),
    );
    if let Some(n) = content.content_length {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    response
}

pub async fn content(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    query: Result<Query<ContentQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(q)) = query else {
        return job_error(InferenceError::InvalidRequest.into());
    };
    let asset = match q.variant.as_deref().map(VideoAsset::parse) {
        None => VideoAsset::Video,
        Some(Some(a)) => a,
        Some(None) => return job_error(InferenceError::InvalidRequest.into()),
    };
    match Jobs::new(store, &engine)
        .video_content(&principal, &id, asset)
        .await
    {
        Ok(content) => stream_response(content),
        Err(e) => job_error(e),
    }
}

pub async fn delete(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match Jobs::new(store, &engine)
        .delete_video(&principal, &id)
        .await
    {
        Ok(job) => Json(json!({
            "id": client_id(JobKind::Video.prefix(), job.id),
            "object": "video.deleted",
            "deleted": true,
        }))
        .into_response(),
        Err(e) => job_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers(ct: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, ct.parse().unwrap());
        h
    }
    #[test]
    fn requests_are_strict_and_explicit() {
        let json = |v: Value| parse(&headers("application/json"), v.to_string().as_bytes());
        let r = json(json!({"model":"vid","prompt":"a cat"})).unwrap();
        assert_eq!((r.seconds, r.size), (4, VideoSize::P720x1280));
        let r = json(json!({"model":"vid","prompt":"a cat","seconds":"12","size":"1792x1024"}))
            .unwrap();
        assert_eq!((r.seconds, r.size), (12, VideoSize::P1792x1024));
        assert!(json(json!({"model":"vid","prompt":"a","seconds":8})).is_ok());
        for bad in [
            json!({"prompt":"a"}),
            json!({"model":"vid"}),
            json!({"model":"vid","prompt":"a","seconds":"5"}),
            json!({"model":"vid","prompt":"a","size":"auto"}),
            json!({"model":"vid","prompt":"a","extra":1}),
            json!({"model":"vid","prompt":" "}),
        ] {
            assert_eq!(json(bad).err(), Some(InferenceError::InvalidRequest));
        }
        assert_eq!(
            json(json!({"model":"vid","prompt":"a","input_reference":{"image_url":"x"}})).err(),
            Some(InferenceError::Unsupported)
        );
        let form = "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nvid\r\n--B\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\na dog\r\n--B\r\nContent-Disposition: form-data; name=\"seconds\"\r\n\r\n8\r\n--B--\r\n";
        let r = parse(&headers("multipart/form-data; boundary=B"), form.as_bytes()).unwrap();
        assert_eq!((r.model.as_str(), r.seconds), ("vid", 8));
        let reference = "--B\r\nContent-Disposition: form-data; name=\"input_reference\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n--B--\r\n";
        assert_eq!(
            parse(
                &headers("multipart/form-data; boundary=B"),
                reference.as_bytes()
            )
            .err(),
            Some(InferenceError::Unsupported)
        );
        assert!(parse(&headers("text/plain"), b"x").is_err());
    }
}
