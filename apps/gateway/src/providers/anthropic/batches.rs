//! Anthropic Message Batches (`POST /v1/messages/batches`, version
//! 2023-06-01) for native gateway batches (`jobs::native`):
//! - create: one JSON body `{"requests":[{custom_id, params}, …]}`, streamed
//!   record by record (an error item aborts the body);
//! - retrieve / cancel: `GET /messages/batches/{id}`, `POST …/{id}/cancel`;
//! - results: `GET …/{id}/results` (JSONL) once `processing_status` is
//!   `ended`; delete: `DELETE …/{id}`.
//!
//! Lines are encoded with the same canonical translation as interactive
//! requests (`super::encode`) and results decoded with `super::decode`.
//! Upstream `custom_id`s are the gateway's `l<n>`; error messages are never
//! read (codes only); bodies are never logged.
use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{AnthropicAdapter, decode, encode};
use crate::{
    inference::{error::InferenceError, types::Deployment},
    jobs::types::*,
    providers::framing::{body, status, transport},
};

type Result<T> = std::result::Result<T, InferenceError>;

pub(super) fn native_batch(target: &Deployment, endpoint: BatchEndpoint) -> bool {
    target.provider == "anthropic"
        && target.credential_ref != "none"
        && target.endpoint.is_none()
        && target.region.as_deref().is_none_or(str::is_empty)
        && !target.upstream_model.trim().is_empty()
        && endpoint.is_generation()
}

fn key(adapter: &AnthropicAdapter, target: &Deployment) -> Result<reqwest::header::HeaderValue> {
    if !native_batch(target, BatchEndpoint::Messages) {
        return Err(InferenceError::Configuration);
    }
    let secret = adapter.resolver.resolve(&target.credential_ref)?;
    let mut key = reqwest::header::HeaderValue::from_str(secret.expose())
        .map_err(|_| InferenceError::Configuration)?;
    key.set_sensitive(true);
    Ok(key)
}

pub(super) fn encode_native_line(
    target: &Deployment,
    endpoint: BatchEndpoint,
    custom_id: &str,
    request: &BatchRequest,
) -> Result<Vec<u8>> {
    let BatchRequest::Chat(r) = request else {
        return Err(InferenceError::Unsupported);
    };
    if !endpoint.is_generation() || r.stream {
        return Err(InferenceError::InvalidRequest);
    }
    let mut params = encode(&target.upstream_model, r)?;
    // Batch params never stream.
    if let Some(fields) = params.as_object_mut() {
        fields.remove("stream");
    }
    serde_json::to_vec(&json!({"custom_id": custom_id, "params": params}))
        .map_err(|_| InferenceError::InvalidRequest)
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp())
}

/// A `message_batch` object in the gateway's batch vocabulary.
pub(super) fn parse_batch(value: &Value) -> Result<UpstreamBatch> {
    if value["type"] != "message_batch" {
        return Err(InferenceError::InvalidUpstream);
    }
    let id = value["id"]
        .as_str()
        .and_then(UpstreamId::parse)
        .ok_or(InferenceError::InvalidUpstream)?;
    let c = &value["request_counts"];
    let n = |k: &str| {
        c[k].as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(InferenceError::InvalidUpstream)
    };
    let (processing, succeeded, errored, canceled, expired) = (
        n("processing")?,
        n("succeeded")?,
        n("errored")?,
        n("canceled")?,
        n("expired")?,
    );
    let total = [processing, succeeded, errored, canceled, expired]
        .into_iter()
        .try_fold(0u32, u32::checked_add)
        .ok_or(InferenceError::InvalidUpstream)?;
    let failed = errored
        .checked_add(canceled)
        .and_then(|n| n.checked_add(expired))
        .ok_or(InferenceError::InvalidUpstream)?;
    let cancel_initiated = timestamp(&value["cancel_initiated_at"]);
    let ended = timestamp(&value["ended_at"]);
    let status = match value["processing_status"].as_str() {
        Some("in_progress") => BatchStatus::InProgress,
        Some("canceling") => BatchStatus::Cancelling,
        Some("ended") if cancel_initiated.is_some() => BatchStatus::Cancelled,
        Some("ended") if total > 0 && expired == total => BatchStatus::Expired,
        Some("ended") => BatchStatus::Completed,
        _ => return Err(InferenceError::InvalidUpstream),
    };
    Ok(UpstreamBatch {
        id,
        status,
        input_file: None,
        output_file: None,
        error_file: None,
        counts: Some(RequestCounts {
            total,
            completed: succeeded,
            failed,
        }),
        // Message Batches report no aggregate usage: it is summed per line.
        usage: None,
        created_at: timestamp(&value["created_at"]),
        in_progress_at: timestamp(&value["created_at"]),
        finalizing_at: None,
        completed_at: ended.filter(|_| status == BatchStatus::Completed),
        failed_at: None,
        expired_at: ended.filter(|_| status == BatchStatus::Expired),
        cancelling_at: cancel_initiated,
        cancelled_at: ended.filter(|_| status == BatchStatus::Cancelled),
        expires_at: timestamp(&value["expires_at"]),
        metadata: None,
    })
}

pub(super) async fn submit_native_batch(
    adapter: &AnthropicAdapter,
    target: &Deployment,
    endpoint: BatchEndpoint,
    records: ByteStream,
) -> Result<UpstreamBatch> {
    if !native_batch(target, endpoint) {
        return Err(InferenceError::Configuration);
    }
    let key = key(adapter, target)?;
    let mut first = true;
    let elements = records.map(move |record| {
        record.map(|bytes| {
            let mut out = Vec::with_capacity(bytes.len() + 1);
            if !std::mem::take(&mut first) {
                out.push(b',');
            }
            out.extend_from_slice(&bytes);
            Bytes::from(out)
        })
    });
    let payload = futures_util::stream::once(async { Ok(Bytes::from_static(b"{\"requests\":[")) })
        .chain(elements)
        .chain(futures_util::stream::once(async {
            Ok(Bytes::from_static(b"]}"))
        }));
    let response = adapter
        .client
        .post(format!("{}/messages/batches", adapter.base))
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::wrap_stream(payload))
        .send()
        .await
        .map_err(transport)?;
    status(response.status())?;
    parse_batch(&body(response).await?)
}

async fn batch_request(
    adapter: &AnthropicAdapter,
    target: &Deployment,
    batch: &UpstreamId,
    method: reqwest::Method,
    suffix: &str,
) -> Result<reqwest::Response> {
    let key = key(adapter, target)?;
    let response = adapter
        .client
        .request(
            method,
            format!(
                "{}/messages/batches/{}{suffix}",
                adapter.base,
                batch.as_str()
            ),
        )
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", key)
        .send()
        .await
        .map_err(transport)?;
    Ok(response)
}

pub(super) async fn batch_action(
    adapter: &AnthropicAdapter,
    target: &Deployment,
    batch: &UpstreamId,
    cancel: bool,
) -> Result<UpstreamBatch> {
    let (method, suffix) = if cancel {
        (reqwest::Method::POST, "/cancel")
    } else {
        (reqwest::Method::GET, "")
    };
    let response = batch_request(adapter, target, batch, method, suffix).await?;
    status(response.status())?;
    let parsed = parse_batch(&body(response).await?)?;
    if &parsed.id != batch {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(parsed)
}

const RESULT_TYPES: &[&str] = &[
    "application/x-jsonl",
    "application/jsonl",
    "application/x-ndjson",
    "application/octet-stream",
    "application/binary",
    "text/plain",
];

pub(super) async fn native_batch_results(
    adapter: &AnthropicAdapter,
    target: &Deployment,
    batch: &UpstreamBatch,
) -> Result<ByteStream> {
    let response =
        batch_request(adapter, target, &batch.id, reqwest::Method::GET, "/results").await?;
    status(response.status())?;
    let media =
        crate::providers::audio::media_type(&response).ok_or(InferenceError::InvalidUpstream)?;
    if !RESULT_TYPES.contains(&media.as_str()) {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(Box::pin(
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(transport)),
    ))
}

/// `{custom_id, result:{type, message | error}}`. Error messages are never
/// read; the nested error `type` becomes the code.
pub(super) fn decode_native_result(endpoint: BatchEndpoint, line: &[u8]) -> Result<NativeResult> {
    if !endpoint.is_generation() {
        return Err(InferenceError::Unsupported);
    }
    let value: Value = serde_json::from_slice(line).map_err(|_| InferenceError::InvalidUpstream)?;
    let custom_id = value["custom_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .ok_or(InferenceError::InvalidUpstream)?
        .to_owned();
    let result = &value["result"];
    let outcome = match result["type"].as_str() {
        Some("succeeded") => {
            NativeOutcome::Succeeded(Box::new(BatchResponse::Chat(decode(&result["message"])?)))
        }
        Some("errored") => {
            let error = &result["error"]["error"];
            let code = error["type"].as_str().unwrap_or("");
            let status = match code {
                "invalid_request_error" => 400,
                "authentication_error" => 401,
                "permission_error" => 403,
                "not_found_error" => 404,
                "request_too_large" => 413,
                "rate_limit_error" => 429,
                "overloaded_error" => 529,
                _ => 500,
            };
            NativeOutcome::Failed {
                status,
                code: ErrorCode::parse(code),
            }
        }
        Some("canceled") => NativeOutcome::Cancelled,
        Some("expired") => NativeOutcome::Expired,
        _ => return Err(InferenceError::InvalidUpstream),
    };
    Ok(NativeResult { custom_id, outcome })
}

pub(super) async fn delete_native_batch(
    adapter: &AnthropicAdapter,
    target: &Deployment,
    batch: &UpstreamBatch,
) -> Result<()> {
    let response = batch_request(adapter, target, &batch.id, reqwest::Method::DELETE, "").await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    status(response.status())
}

#[cfg(test)]
mod tests;
