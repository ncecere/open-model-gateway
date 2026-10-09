//! OpenAI async jobs at the fixed origin (shapes from the official OpenAPI
//! spec via openai-python, 2026-10-08):
//! - Videos: `POST /videos` (multipart `model, prompt, seconds, size`; the
//!   gateway builds the form itself and always sends explicit seconds/size),
//!   `GET /videos/{id}`, `DELETE /videos/{id}`, `GET /videos/{id}/content`.
//!   OpenAI shut down the Sora 2 models and this API on 2026-09-24, so the
//!   adapter no longer offers the `videos` protocol: new jobs are refused
//!   before admission. These calls remain for jobs created earlier.
//! - Files + Batch: `POST /files` (multipart `purpose=batch` + a streamed
//!   JSONL `file`, chunked), `POST /batches`, `GET /batches/{id}`,
//!   `POST /batches/{id}/cancel`, `GET /files/{id}/content`.
//!
//! Upstream ids are validated (`[A-Za-z0-9._:-]{1,128}`) before they are put
//! in a path. Error bodies are never read; job error messages are dropped
//! (codes only). Content bodies are streamed, never buffered or logged.
use futures_util::StreamExt;
use reqwest::header::{self, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};

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
/// One batch output line (a full chat completion) may be large.
const OUTPUT_LINE_LIMIT: usize = 16 * 1024 * 1024;

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

pub(super) async fn upload_batch_file(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    content: ByteStream,
) -> Result<UpstreamFile> {
    let auth = authorization(adapter, target)?;
    let boundary = format!("omg-{}", uuid::Uuid::new_v4().simple());
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"batch.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n"
    );
    let tail = format!("\r\n--{boundary}--\r\n");
    // A content error (validation failure, client abort) aborts the request
    // body, so the provider never receives a complete form.
    let body = futures_util::stream::once(async move { Ok(axum::body::Bytes::from(head)) })
        .chain(content)
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
    Ok(UpstreamFile {
        id: id(&value["id"])?,
        bytes: value["bytes"].as_u64(),
    })
}

/// Inclusive OpenAI token usage whose schema defines only cached reads (the
/// Batch `usage` object and a batch line's Chat `usage`): like the Images
/// API, cache writes are not a category of this schema, so they are zero;
/// a missing `cached_tokens` is zero for the same reason. Both totals are
/// required; malformed usage is unknown, never zero.
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
        .is_some_and(|e| e != BATCH_ENDPOINT)
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

pub(super) async fn create_batch(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    input: &UpstreamId,
    metadata: Option<Map<String, Value>>,
) -> Result<UpstreamBatch> {
    let auth = authorization(adapter, target)?;
    let mut body = json!({
        "input_file_id": input.as_str(),
        "endpoint": BATCH_ENDPOINT,
        "completion_window": BATCH_COMPLETION_WINDOW,
    });
    if let Some(m) = metadata {
        if !valid_metadata(&m) {
            return Err(InferenceError::InvalidRequest);
        }
        body["metadata"] = Value::Object(m);
    }
    let response = adapter
        .client
        .post(format!("{}/batches", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .json(&body)
        .send()
        .await
        .map_err(transport_error)?;
    parse_batch(&read_object(response).await?)
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

pub(super) async fn file_content(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    file: &UpstreamId,
) -> Result<ContentStream> {
    let response = open_file(adapter, target, file).await?;
    Ok(stream(response, "application/jsonl"))
}

#[derive(Deserialize)]
struct OutputLine {
    response: Option<OutputResponse>,
}
#[derive(Deserialize)]
struct OutputResponse {
    status_code: Option<u16>,
    body: Option<OutputBody>,
}
#[derive(Deserialize)]
struct OutputBody {
    usage: Option<Value>,
}

/// Sum of two observations; any unknown counter stays unknown.
fn add(a: Usage, b: Usage) -> Usage {
    let sum = |x: Option<u64>, y: Option<u64>| x.zip(y).and_then(|(x, y)| x.checked_add(y));
    let billing = a
        .billing
        .zip(b.billing)
        .map(|(x, y)| crate::billing::BillingUsage {
            total_input_tokens: sum(x.total_input_tokens, y.total_input_tokens),
            uncached_input_tokens: sum(x.uncached_input_tokens, y.uncached_input_tokens),
            cache_read_input_tokens: sum(x.cache_read_input_tokens, y.cache_read_input_tokens),
            cache_write_input_tokens: sum(x.cache_write_input_tokens, y.cache_write_input_tokens),
            cache_write_default_input_tokens: sum(
                x.cache_write_default_input_tokens,
                y.cache_write_default_input_tokens,
            ),
            cache_write_5m_input_tokens: sum(
                x.cache_write_5m_input_tokens,
                y.cache_write_5m_input_tokens,
            ),
            cache_write_1h_input_tokens: sum(
                x.cache_write_1h_input_tokens,
                y.cache_write_1h_input_tokens,
            ),
        });
    Usage {
        input_tokens: sum(a.input_tokens, b.input_tokens),
        output_tokens: sum(a.output_tokens, b.output_tokens),
        billing,
        reasoning_tokens: sum(a.reasoning_tokens, b.reasoning_tokens),
        ..Usage::default()
    }
}

/// Stream the output file and sum each line's `response.body.usage`
/// (Chat Completions counters). Bodies are parsed only for usage and
/// dropped line by line; nothing is stored. Any line without valid usage,
/// an oversized line, or more than `max_bytes` makes the usage unknown.
pub(super) async fn output_usage(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    file: &UpstreamId,
    max_bytes: u64,
) -> Result<OutputUsage> {
    let response = open_file(adapter, target, file).await?;
    let mut chunks = response.bytes_stream();
    let mut line = Vec::new();
    let mut lines = 0u64;
    let mut total = 0u64;
    let mut usage: Option<Usage> = None;
    let mut unknown = false;
    let mut consume = |line: &[u8], lines: &mut u64| {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        *lines += 1;
        let parsed = serde_json::from_slice::<OutputLine>(line)
            .ok()
            .and_then(|l| l.response)
            .filter(|r| r.status_code == Some(200))
            .and_then(|r| r.body)
            .and_then(|b| b.usage)
            .and_then(|u| {
                schema_usage(
                    &u,
                    "prompt_tokens",
                    "completion_tokens",
                    "prompt_tokens_details",
                )
            });
        match parsed {
            Some(u) if !unknown => usage = Some(usage.map_or(u, |a| add(a, u))),
            _ => unknown = true,
        }
    };
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(transport_error)?;
        total = total.saturating_add(chunk.len() as u64);
        if total > max_bytes {
            return Ok(OutputUsage { lines, usage: None });
        }
        let mut rest: &[u8] = &chunk;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&rest[..pos]);
            if line.len() > OUTPUT_LINE_LIMIT {
                return Ok(OutputUsage { lines, usage: None });
            }
            consume(&line, &mut lines);
            line.clear();
            rest = &rest[pos + 1..];
        }
        line.extend_from_slice(rest);
        if line.len() > OUTPUT_LINE_LIMIT {
            return Ok(OutputUsage { lines, usage: None });
        }
    }
    consume(&line, &mut lines);
    drop(line);
    Ok(OutputUsage {
        lines,
        usage: if unknown { None } else { usage },
    })
}

#[cfg(test)]
mod tests;
