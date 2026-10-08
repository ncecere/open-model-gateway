//! OpenAI speech endpoints at the fixed origin:
//! - `POST /audio/transcriptions`: the gateway re-encodes its own multipart
//!   body (fixed field set, `audio.<ext>` filename, `response_format:"json"`
//!   upstream so usage is returned). Client headers, filenames and
//!   credentials are never forwarded.
//! - `POST /audio/speech`: JSON request; the binary body is passed through
//!   (media type checked against the requested format).
//!
//! Error bodies are never read.
use reqwest::header::{self, HeaderValue};
use serde_json::{Value, json};

use super::{BASE, OpenAiAdapter, check_status, transport_error};
use crate::{
    inference::{error::InferenceError, types::*},
    providers::audio::{Multipart, decode_transcription, read_json, speech_body},
};

type Result<T> = std::result::Result<T, InferenceError>;

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

pub(super) async fn transcribe(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    request: TranscriptionRequest,
) -> Result<TranscriptionResponse> {
    request.validate()?;
    let auth = authorization(adapter, target)?;
    let mut form = Multipart::new(request.audio.len());
    form.text("model", &target.upstream_model);
    form.text("response_format", "json");
    if let Some(language) = &request.language {
        form.text("language", language);
    }
    if let Some(prompt) = &request.prompt {
        form.text("prompt", prompt);
    }
    if let Some(temperature) = request.temperature {
        form.text("temperature", &temperature.to_string());
    }
    form.file(
        request.format.extension(),
        request.format.mime(),
        &request.audio,
    );
    let (content_type, body) = form.finish();
    let response = adapter
        .client
        .post(format!("{}/audio/transcriptions", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(response.status())?;
    let bytes = read_json(response, transport_error).await?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| InferenceError::InvalidUpstream)?;
    if !(value["error"].is_null()) {
        return Err(InferenceError::InvalidUpstream);
    }
    decode_transcription(&bytes, &value, &request)
}

pub(super) async fn speak(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    request: SpeechRequest,
) -> Result<SpeechResponse> {
    request.validate()?;
    let auth = authorization(adapter, target)?;
    let mut body = json!({
        "model": target.upstream_model,
        "input": request.input,
        "voice": request.voice,
        "response_format": request.response_format.as_str(),
    });
    if let Some(speed) = request.speed {
        body["speed"] = json!(speed);
    }
    let response = adapter
        .client
        .post(format!("{}/audio/speech", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .json(&body)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(response.status())?;
    speech_body(response, &request, transport_error).await
}

#[cfg(test)]
mod tests;
