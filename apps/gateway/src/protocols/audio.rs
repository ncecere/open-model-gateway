//! OpenAI-compatible speech subset (inference keys only):
//!
//! - `POST /v1/audio/transcriptions` (multipart): `file` (one part with a
//!   filename; flac/mp3/mp4/mpeg/mpga/m4a/ogg/wav/webm, content type and
//!   container magic must agree), `model`, `language?`, `prompt?`,
//!   `response_format` `json` (default) or `text`, `temperature?` (0–1).
//!   `verbose_json`/`srt`/`vtt`, timestamp granularities, `include[]`,
//!   chunking and streaming are `unsupported_capability`; other fields are
//!   invalid. The file part is capped separately from the body
//!   (`GATEWAY_AUDIO_MAX_UPLOAD_BYTES`, default 25 MiB → 413).
//! - `POST /v1/audio/speech` (JSON): `{model, input, voice,
//!   response_format? mp3|wav|opus|pcm (default mp3), speed? 0.25–4}` →
//!   binary audio with an explicit `Content-Type`, streamed from upstream.
//!   `input` is at most `GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS` Unicode scalar
//!   values (default 4096). `instructions` and `stream_format:"sse"` are
//!   unsupported; unknown fields are rejected.
//!
//! Filenames, uploads, transcripts and speech text are never logged or echoed.
pub mod multipart;

use std::sync::Arc;

use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::rejection::{BytesRejection, JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::Principal,
    http::RequestId,
    inference::{
        Engine,
        error::InferenceError,
        types::{
            AudioFormat, SpeechFormat, SpeechRequest, TranscriptionFormat, TranscriptionRequest,
            TranscriptionResponse,
        },
    },
};

const MAX_PARTS: usize = 16;
const MAX_FIELD_BYTES: usize = 4096;

fn error(e: InferenceError) -> Response {
    super::workload_error(e, StatusCode::BAD_REQUEST)
}

enum Rejection {
    Error(InferenceError),
    TooLarge,
}
impl From<InferenceError> for Rejection {
    fn from(e: InferenceError) -> Self {
        Self::Error(e)
    }
}
impl Rejection {
    fn response(self) -> Response {
        match self {
            Self::Error(e) => error(e),
            Self::TooLarge => super::payload_too_large(),
        }
    }
}

/// The lowercase extension of the last path component of a client filename.
fn extension(filename: &str) -> Option<String> {
    let base = filename.rsplit(['/', '\\']).next()?;
    let (_, ext) = base.rsplit_once('.')?;
    (!ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| ext.to_ascii_lowercase())
}

fn text_field(data: &[u8]) -> Result<String, InferenceError> {
    if data.len() > MAX_FIELD_BYTES {
        return Err(InferenceError::InvalidRequest);
    }
    String::from_utf8(data.to_vec()).map_err(|_| InferenceError::InvalidRequest)
}

/// Strictly parse the multipart form into a canonical request.
fn parse_transcription(
    headers: &HeaderMap,
    body: &[u8],
    max_upload: usize,
) -> Result<TranscriptionRequest, Rejection> {
    let invalid = InferenceError::InvalidRequest;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or(invalid)?;
    let boundary = multipart::boundary(content_type)?;
    let parts = multipart::parse(body, &boundary, MAX_PARTS)?;
    let mut file = None;
    let mut fields = std::collections::BTreeMap::new();
    for part in parts {
        if part.name == "file" {
            let filename = part.filename.as_deref().ok_or(invalid)?;
            if file.is_some() {
                return Err(invalid.into());
            }
            if part.data.len() > max_upload {
                return Err(Rejection::TooLarge);
            }
            if part.data.is_empty() {
                return Err(invalid.into());
            }
            let by_extension = extension(filename)
                .as_deref()
                .and_then(AudioFormat::from_extension)
                .ok_or(invalid)?;
            let format = match part.content_type.as_deref() {
                None => by_extension,
                Some(t) => match AudioFormat::from_content_type(t).ok_or(invalid)? {
                    None => by_extension,
                    Some(f) if f == by_extension => f,
                    Some(_) => return Err(invalid.into()),
                },
            };
            file = Some((format, part.data));
            continue;
        }
        if part.filename.is_some() {
            return Err(invalid.into());
        }
        let value = text_field(part.data)?;
        match part.name.as_str() {
            "model" | "language" | "prompt" | "response_format" | "temperature" => {}
            "stream" if value == "false" => continue,
            "stream"
            | "timestamp_granularities"
            | "timestamp_granularities[]"
            | "include"
            | "include[]"
            | "chunking_strategy"
            | "known_speaker_names[]"
            | "known_speaker_references[]" => {
                return Err(InferenceError::Unsupported.into());
            }
            _ => return Err(invalid.into()),
        }
        if fields.insert(part.name, value).is_some() {
            return Err(invalid.into());
        }
    }
    let (format, data) = file.ok_or(invalid)?;
    let model = fields.remove("model").ok_or(invalid)?;
    let response_format = match fields.remove("response_format").as_deref() {
        None | Some("json") => TranscriptionFormat::Json,
        Some("text") => TranscriptionFormat::Text,
        Some("verbose_json" | "srt" | "vtt" | "diarized_json") => {
            return Err(InferenceError::Unsupported.into());
        }
        Some(_) => return Err(invalid.into()),
    };
    let temperature = fields
        .remove("temperature")
        .map(|t| t.trim().parse::<f64>().map_err(|_| invalid))
        .transpose()?;
    let request = TranscriptionRequest::new(
        model,
        Arc::from(data),
        format,
        fields.remove("language"),
        fields.remove("prompt"),
        temperature,
        response_format,
    );
    request.validate()?;
    Ok(request)
}

/// OpenAI-shaped usage from observed counters only (never fabricated).
fn transcription_usage(response: &TranscriptionResponse) -> Option<Value> {
    let usage = response.usage;
    if let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) {
        return Some(json!({
            "type": "tokens",
            "input_tokens": input,
            "output_tokens": output,
            "total_tokens": input.saturating_add(output),
        }));
    }
    let ms = usage.meters.and_then(|m| m.input_audio_seconds_ms)?;
    Some(json!({"type": "duration", "seconds": ms as f64 / 1000.0}))
}

pub async fn transcriptions(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return error(InferenceError::InvalidRequest),
    };
    let request = match parse_transcription(&headers, &body, engine.limits().audio.max_upload_bytes)
    {
        Ok(request) => request,
        Err(rejection) => return rejection.response(),
    };
    drop(body);
    let format = request.response_format;
    match engine
        .execute_transcription(principal, request, request_id.0)
        .await
    {
        Ok(response) => match format {
            TranscriptionFormat::Text => (
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                )],
                response.text,
            )
                .into_response(),
            TranscriptionFormat::Json => {
                let mut body = json!({"text": response.text});
                if let Some(usage) = transcription_usage(&response) {
                    body["usage"] = usage;
                }
                Json(body).into_response()
            }
        },
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechWire {
    model: String,
    input: String,
    voice: String,
    response_format: Option<String>,
    speed: Option<f64>,
    // Recognized OpenAI options outside the subset: rejected explicitly.
    instructions: Option<Value>,
    stream_format: Option<String>,
}
impl SpeechWire {
    fn normalize(self) -> Result<SpeechRequest, InferenceError> {
        if self.instructions.is_some()
            || self.stream_format.as_deref().is_some_and(|f| f != "audio")
        {
            return Err(InferenceError::Unsupported);
        }
        let response_format = match self.response_format.as_deref() {
            None => SpeechFormat::Mp3,
            Some(f) => match SpeechFormat::parse(f) {
                Some(f) => f,
                None if matches!(f, "aac" | "flac") => return Err(InferenceError::Unsupported),
                None => return Err(InferenceError::InvalidRequest),
            },
        };
        let request = SpeechRequest {
            model: self.model,
            input: self.input,
            voice: self.voice,
            response_format,
            speed: self.speed,
        };
        request.validate()?;
        Ok(request)
    }
}

pub async fn speech(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    input: Result<Json<SpeechWire>, JsonRejection>,
) -> Response {
    let request = match input {
        Ok(Json(wire)) => match wire.normalize() {
            Ok(request) => request,
            Err(e) => return error(e),
        },
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return error(InferenceError::InvalidRequest),
    };
    match engine
        .execute_speech(principal, request, request_id.0)
        .await
    {
        Ok(response) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(response.content_type),
            )],
            Body::from_stream(response.audio),
        )
            .into_response(),
        Err(e) => error(e),
    }
}

#[cfg(test)]
mod tests;
