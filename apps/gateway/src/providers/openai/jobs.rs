//! OpenAI async jobs at the fixed origin (shapes from the official OpenAPI
//! spec via openai-python, 2026-10-08):
//! - Videos: `POST /videos` (multipart `model, prompt, seconds, size`; the
//!   gateway builds the form itself and always sends explicit seconds/size),
//!   `GET /videos/{id}`, `DELETE /videos/{id}`, `GET /videos/{id}/content`.
//!   OpenAI shut down the Sora 2 models and this API on 2026-09-24, so the
//!   adapter no longer offers the `videos` protocol: new jobs are refused
//!   before admission. These calls remain for jobs created earlier.
//! - Native batches (`jobs::native`): `POST /files` (multipart
//!   `purpose=batch` + a streamed JSONL `file` the gateway encodes), `POST
//!   /batches`, `GET /batches/{id}`, `POST /batches/{id}/cancel`, `GET
//!   /files/{id}/content` (output and error files) and `DELETE /files/{id}`.
//!
//! Upstream ids are validated (`[A-Za-z0-9._:-]{1,128}`) before they are put
//! in a path. Error bodies are never read; job error messages are dropped
//! (codes only). Content bodies are streamed, never buffered or logged.
use futures_util::StreamExt;
use reqwest::header::{self, HeaderValue};
use serde_json::{Value, json};

use super::{BASE, OpenAiAdapter, check_status, transport_error};
use crate::{
    billing::MeterVariant,
    inference::{
        error::InferenceError,
        types::{Deployment, Usage},
    },
    jobs::types::*,
    providers::{audio::Multipart, metering},
};

type Result<T> = std::result::Result<T, InferenceError>;

/// Job/file objects are small JSON documents.
const OBJECT_LIMIT: usize = 1024 * 1024;

/// Validate the connection before resolving credentials.
fn authorization(adapter: &OpenAiAdapter, target: &Deployment) -> Result<HeaderValue> {
    if target.provider != "openai"
        || target.credential_ref == "none"
        || target
            .endpoint
            .as_deref()
            .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
        || target.region.as_deref().is_some_and(|v| !v.is_empty())
        || target.upstream_model.trim().is_empty()
    {
        return Err(InferenceError::Configuration);
    }
    let secret = adapter.resolver.resolve(&target.credential_ref)?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        .map_err(|_| InferenceError::Configuration)?;
    auth.set_sensitive(true);
    Ok(auth)
}

fn media_type(response: &reqwest::Response) -> Option<String> {
    crate::providers::audio::media_type(response)
}

/// Bounded JSON object (status already checked).
async fn read_object(response: reqwest::Response) -> Result<Value> {
    check_status(response.status())?;
    if media_type(&response).as_deref() != Some("application/json")
        || response
            .content_length()
            .is_some_and(|n| n > OBJECT_LIMIT as u64)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if chunk.len() > OBJECT_LIMIT - body.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| InferenceError::InvalidUpstream)?;
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(value)
}

fn object_kind(value: &Value, kind: &str) -> Result<()> {
    match &value["object"] {
        Value::Null => Ok(()),
        Value::String(s) if s == kind => Ok(()),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
fn id(value: &Value) -> Result<UpstreamId> {
    value
        .as_str()
        .and_then(UpstreamId::parse)
        .ok_or(InferenceError::InvalidUpstream)
}
fn optional_id(value: &Value) -> Result<Option<UpstreamId>> {
    match value {
        Value::Null => Ok(None),
        v => id(v).map(Some),
    }
}
/// Unix seconds; anything else is unknown (never guessed).
fn timestamp(value: &Value) -> Option<i64> {
    value.as_i64().filter(|n| *n >= 0)
}

// ---------------------------------------------------------------- Video ----

/// The Sora family only; other upstream models are not this profile.
pub(super) fn supports_video(target: &Deployment, request: &VideoRequest) -> bool {
    target.upstream_model.starts_with("sora-") && request.validate().is_ok()
}

pub(super) fn parse_video(value: &Value) -> Result<UpstreamVideo> {
    object_kind(value, "video")?;
    let state = match value["status"].as_str() {
        Some("queued") => JobState::Queued,
        Some("in_progress") => JobState::InProgress,
        Some("completed") => JobState::Completed,
        Some("failed") => JobState::Failed,
        _ => return Err(InferenceError::InvalidUpstream),
    };
    // `seconds` is a decimal string ("8"); accept a whole JSON number too.
    let seconds = match &value["seconds"] {
        Value::String(s)
            if !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit()) =>
        {
            s.parse().ok()
        }
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        _ => None,
    }
    .filter(|n| (1..=3600).contains(n));
    let error = match &value["error"] {
        Value::Null => None,
        Value::Object(e) => Some(ErrorCode::parse(
            e.get("code").and_then(Value::as_str).unwrap_or(""),
        )),
        _ => return Err(InferenceError::InvalidUpstream),
    };
    Ok(UpstreamVideo {
        id: id(&value["id"])?,
        state,
        progress: value["progress"]
            .as_u64()
            .filter(|n| *n <= 100)
            .map(|n| n as u8),
        seconds,
        size: value["size"].as_str().and_then(MeterVariant::new),
        completed_at: timestamp(&value["completed_at"]),
        expires_at: timestamp(&value["expires_at"]),
        error,
    })
}

pub(super) async fn create_video(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    request: VideoRequest,
) -> Result<UpstreamVideo> {
    if !supports_video(target, &request) {
        return Err(InferenceError::Unsupported);
    }
    let auth = authorization(adapter, target)?;
    let mut form = Multipart::new(request.prompt.len());
    form.text("model", &target.upstream_model);
    form.text("prompt", &request.prompt);
    form.text("seconds", &request.seconds.to_string());
    form.text("size", request.size.as_str());
    let (content_type, body) = form.finish();
    let response = adapter
        .client
        .post(format!("{}/videos", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(transport_error)?;
    parse_video(&read_object(response).await?)
}

pub(super) async fn retrieve_video(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    video: &UpstreamId,
) -> Result<UpstreamVideo> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .get(format!("{}/videos/{}", adapter.base, video.as_str()))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    let parsed = parse_video(&read_object(response).await?)?;
    if &parsed.id != video {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(parsed)
}

pub(super) async fn delete_video(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    video: &UpstreamId,
) -> Result<()> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .delete(format!("{}/videos/{}", adapter.base, video.as_str()))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    let value = read_object(response).await?;
    object_kind(&value, "video.deleted")?;
    if value["deleted"] != json!(true) || &id(&value["id"])? != video {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}

/// Allowlisted pass-through media types (normalized).
fn content_type(response: &reqwest::Response, allowed: &[&'static str]) -> Result<&'static str> {
    let actual = media_type(response).ok_or(InferenceError::InvalidUpstream)?;
    allowed
        .iter()
        .copied()
        .find(|t| *t == actual)
        .ok_or(InferenceError::InvalidUpstream)
}
fn stream(response: reqwest::Response, media: &'static str) -> ContentStream {
    ContentStream {
        content_type: media,
        content_length: response.content_length(),
        body: Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(transport_error)),
        ),
    }
}

pub(super) async fn video_content(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    video: &UpstreamId,
    asset: VideoAsset,
) -> Result<ContentStream> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .get(format!(
            "{}/videos/{}/content?variant={}",
            adapter.base,
            video.as_str(),
            asset.as_str()
        ))
        .header(header::AUTHORIZATION, auth)
        .header(header::ACCEPT, "application/binary")
        .send()
        .await
        .map_err(transport_error)?;
    check_status(response.status())?;
    let allowed: &[&'static str] = match asset {
        VideoAsset::Video => &[
            "video/mp4",
            "application/octet-stream",
            "application/binary",
        ],
        VideoAsset::Thumbnail | VideoAsset::Spritesheet => &[
            "image/webp",
            "image/jpeg",
            "image/png",
            "application/octet-stream",
            "application/binary",
        ],
    };
    let media = content_type(&response, allowed)?;
    // Generic binary types are labeled by the requested asset.
    let media = match (media, asset) {
        ("application/octet-stream" | "application/binary", VideoAsset::Video) => "video/mp4",
        ("application/octet-stream" | "application/binary", _) => "application/octet-stream",
        (m, _) => m,
    };
    Ok(stream(response, media))
}

// ---------------------------------------------------------------- Batch ----
//
// Native batches (`jobs::native`): the gateway encodes each validated line
// itself (`custom_id` = the gateway's `l<n>`, model = the upstream model),
// uploads the JSONL as one streamed multipart file, creates the batch, and
// later downloads the output and error files, decodes each line into a
// canonical result and deletes the provider's files.

/// The upstream URL of an endpoint's lines: chat-like lines are sent as
/// Chat Completions, embeddings as Embeddings.
fn upstream_url(endpoint: BatchEndpoint) -> &'static str {
    if endpoint.is_generation() {
        "/v1/chat/completions"
    } else {
        "/v1/embeddings"
    }
}

/// Connection shape accepted for native batches (before any secret lookup).
pub(super) fn native_batch(target: &Deployment) -> bool {
    target.provider == "openai"
        && target.credential_ref != "none"
        && target
            .endpoint
            .as_deref()
            .is_none_or(|v| v == BASE || v == "https://api.openai.com/v1/")
        && target.region.as_deref().is_none_or(str::is_empty)
        && !target.upstream_model.trim().is_empty()
}

pub(super) fn encode_native_line(
    target: &Deployment,
    endpoint: BatchEndpoint,
    custom_id: &str,
    request: &BatchRequest,
) -> Result<Vec<u8>> {
    let body = match (request, endpoint.is_generation()) {
        (BatchRequest::Chat(r), true) if !r.stream => super::encode(&target.upstream_model, r),
        (BatchRequest::Embeddings(r), false) => {
            crate::providers::embeddings::validate(r)?;
            crate::providers::embeddings::encode(&target.upstream_model, r)
        }
        _ => return Err(InferenceError::InvalidRequest),
    };
    serde_json::to_vec(&json!({
        "custom_id": custom_id,
        "method": "POST",
        "url": upstream_url(endpoint),
        "body": body,
    }))
    .map_err(|_| InferenceError::InvalidRequest)
}

/// Stream the records as one JSONL `purpose=batch` file. An error item
/// aborts the request body, so the provider never receives a complete form.
async fn upload_records(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    records: ByteStream,
) -> Result<UpstreamId> {
    let auth = authorization(adapter, target)?;
    let boundary = format!("omg-{}", uuid::Uuid::new_v4().simple());
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"batch.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n"
    );
    let tail = format!("\r\n--{boundary}--\r\n");
    let lines = records.map(|record| {
        record.map(|bytes| {
            let mut line = Vec::with_capacity(bytes.len() + 1);
            line.extend_from_slice(&bytes);
            line.push(b'\n');
            axum::body::Bytes::from(line)
        })
    });
    let body = futures_util::stream::once(async move { Ok(axum::body::Bytes::from(head)) })
        .chain(lines)
        .chain(futures_util::stream::once(async move {
            Ok(axum::body::Bytes::from(tail))
        }));
    let response = adapter
        .client
        .post(format!("{}/files", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .map_err(transport_error)?;
    let value = read_object(response).await?;
    object_kind(&value, "file")?;
    if value["purpose"].as_str().is_some_and(|p| p != "batch") {
        return Err(InferenceError::InvalidUpstream);
    }
    id(&value["id"])
}

pub(super) async fn submit_native_batch(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    endpoint: BatchEndpoint,
    records: ByteStream,
) -> Result<UpstreamBatch> {
    let input = upload_records(adapter, target, records).await?;
    let auth = authorization(adapter, target)?;
    let created = async {
        let response = adapter
            .client
            .post(format!("{}/batches", adapter.base))
            .header(header::AUTHORIZATION, auth)
            .json(&json!({
                "input_file_id": input.as_str(),
                "endpoint": upstream_url(endpoint),
                "completion_window": BATCH_COMPLETION_WINDOW,
            }))
            .send()
            .await
            .map_err(transport_error)?;
        parse_batch(&read_object(response).await?)
    }
    .await;
    match created {
        Ok(mut batch) => {
            batch.input_file.get_or_insert(input);
            Ok(batch)
        }
        Err(e) => {
            // The batch was not created: remove the uploaded copy (best effort).
            let _ = delete_file(adapter, target, &input).await;
            Err(e)
        }
    }
}

pub(super) fn schema_usage(
    value: &Value,
    input_key: &str,
    output_key: &str,
    details_key: &str,
) -> Option<Usage> {
    if !value.is_object() {
        return None;
    }
    let input = metering::count(&value[input_key]).ok()??;
    let output = metering::count(&value[output_key]).ok()??;
    let details = &value[details_key];
    if !(details.is_null() || details.is_object()) {
        return None;
    }
    let cached = metering::count(&details["cached_tokens"])
        .ok()?
        .unwrap_or(0);
    if let Some(total) = metering::count(&value["total_tokens"]).ok()?
        && input.checked_add(output) != Some(total)
    {
        return None;
    }
    let billing = crate::billing::BillingUsage {
        total_input_tokens: Some(input),
        uncached_input_tokens: Some(input.checked_sub(cached)?),
        cache_read_input_tokens: Some(cached),
        cache_write_input_tokens: Some(0),
        cache_write_default_input_tokens: Some(0),
        cache_write_5m_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
    };
    billing.validate().ok()?;
    let reasoning = ["output_tokens_details", "completion_tokens_details"]
        .into_iter()
        .find_map(|k| value[k]["reasoning_tokens"].as_u64())
        .filter(|n| *n <= output);
    Some(Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        billing: Some(billing),
        reasoning_tokens: reasoning,
        ..Usage::default()
    })
}
fn batch_usage(value: &Value) -> Option<Usage> {
    schema_usage(
        value,
        "input_tokens",
        "output_tokens",
        "input_tokens_details",
    )
}

pub(super) fn parse_batch(value: &Value) -> Result<UpstreamBatch> {
    object_kind(value, "batch")?;
    if value["endpoint"]
        .as_str()
        .is_some_and(|e| e != "/v1/chat/completions" && e != "/v1/embeddings")
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let status = value["status"]
        .as_str()
        .and_then(BatchStatus::parse)
        .ok_or(InferenceError::InvalidUpstream)?;
    let counts = match &value["request_counts"] {
        Value::Null => None,
        c => {
            let n = |k: &str| c[k].as_u64().and_then(|n| u32::try_from(n).ok());
            match (n("total"), n("completed"), n("failed")) {
                (Some(total), Some(completed), Some(failed))
                    if u64::from(completed) + u64::from(failed) <= u64::from(total) =>
                {
                    Some(RequestCounts {
                        total,
                        completed,
                        failed,
                    })
                }
                _ => return Err(InferenceError::InvalidUpstream),
            }
        }
    };
    let metadata = value["metadata"]
        .as_object()
        .filter(|m| valid_metadata(m))
        .cloned();
    Ok(UpstreamBatch {
        id: id(&value["id"])?,
        status,
        input_file: optional_id(&value["input_file_id"])?,
        output_file: optional_id(&value["output_file_id"])?,
        error_file: optional_id(&value["error_file_id"])?,
        counts,
        usage: batch_usage(&value["usage"]),
        created_at: timestamp(&value["created_at"]),
        in_progress_at: timestamp(&value["in_progress_at"]),
        finalizing_at: timestamp(&value["finalizing_at"]),
        completed_at: timestamp(&value["completed_at"]),
        failed_at: timestamp(&value["failed_at"]),
        expired_at: timestamp(&value["expired_at"]),
        cancelling_at: timestamp(&value["cancelling_at"]),
        cancelled_at: timestamp(&value["cancelled_at"]),
        expires_at: timestamp(&value["expires_at"]),
        metadata,
    })
}

pub(super) async fn batch_action(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    batch: &UpstreamId,
    cancel: bool,
) -> Result<UpstreamBatch> {
    let auth = authorization(adapter, target)?;
    let url = format!(
        "{}/batches/{}{}",
        adapter.base,
        batch.as_str(),
        if cancel { "/cancel" } else { "" }
    );
    let request = if cancel {
        adapter.client.post(url)
    } else {
        adapter.client.get(url)
    };
    let response = request
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    let parsed = parse_batch(&read_object(response).await?)?;
    if &parsed.id != batch {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(parsed)
}

const FILE_TYPES: &[&str] = &[
    "application/octet-stream",
    "application/jsonl",
    "application/x-ndjson",
    "application/json",
    "text/plain",
];

async fn open_file(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    file: &UpstreamId,
) -> Result<reqwest::Response> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .get(format!("{}/files/{}/content", adapter.base, file.as_str()))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(response.status())?;
    content_type(&response, FILE_TYPES)?;
    Ok(response)
}

/// The output file followed by the error file (a newline between them).
pub(super) async fn native_batch_results(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    batch: &UpstreamBatch,
) -> Result<ByteStream> {
    let mut parts: Vec<ByteStream> = Vec::new();
    for file in [&batch.output_file, &batch.error_file]
        .into_iter()
        .flatten()
    {
        let response = open_file(adapter, target, file).await?;
        if !parts.is_empty() {
            parts.push(Box::pin(futures_util::stream::once(async {
                Ok(axum::body::Bytes::from_static(b"\n"))
            })));
        }
        parts.push(Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(transport_error)),
        ));
    }
    Ok(Box::pin(futures_util::stream::iter(parts).flatten()))
}

/// One output/error file line: `{custom_id, response:{status_code, body},
/// error}`. Error messages are never read; only codes.
pub(super) fn decode_native_result(endpoint: BatchEndpoint, line: &[u8]) -> Result<NativeResult> {
    let value: Value = serde_json::from_slice(line).map_err(|_| InferenceError::InvalidUpstream)?;
    let custom_id = value["custom_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .ok_or(InferenceError::InvalidUpstream)?
        .to_owned();
    let response = &value["response"];
    let outcome = if response.is_object() {
        let status = response["status_code"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| (100..=599).contains(n))
            .ok_or(InferenceError::InvalidUpstream)?;
        let body = &response["body"];
        if status == 200 {
            NativeOutcome::Succeeded(Box::new(if endpoint.is_generation() {
                BatchResponse::Chat(super::decode_complete(body)?)
            } else {
                let n = body["data"].as_array().map_or(0, Vec::len);
                let request = crate::inference::types::EmbeddingRequest {
                    model: String::new(),
                    input: vec![String::from("x"); n],
                    dimensions: None,
                };
                BatchResponse::Embeddings(crate::providers::embeddings::decode(body, &request)?)
            }))
        } else {
            let code = body["error"]["code"]
                .as_str()
                .or_else(|| body["error"]["type"].as_str())
                .unwrap_or("");
            NativeOutcome::Failed {
                status,
                code: ErrorCode::parse(code),
            }
        }
    } else {
        match value["error"]["code"].as_str() {
            Some("batch_expired") => NativeOutcome::Expired,
            Some("batch_cancelled") => NativeOutcome::Cancelled,
            Some(code) => NativeOutcome::Failed {
                status: 500,
                code: ErrorCode::parse(code),
            },
            None => return Err(InferenceError::InvalidUpstream),
        }
    };
    Ok(NativeResult { custom_id, outcome })
}

async fn delete_file(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    file: &UpstreamId,
) -> Result<()> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .delete(format!("{}/files/{}", adapter.base, file.as_str()))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    // Already gone is done.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    check_status(response.status())
}

/// Delete the provider's input, output and error files (batches themselves
/// cannot be deleted on OpenAI). Every file is attempted.
pub(super) async fn delete_native_batch(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    batch: &UpstreamBatch,
) -> Result<()> {
    let mut result = Ok(());
    for file in [&batch.input_file, &batch.output_file, &batch.error_file]
        .into_iter()
        .flatten()
    {
        if let Err(e) = delete_file(adapter, target, file).await {
            result = Err(e);
        }
    }
    result
}

#[cfg(test)]
mod tests;
