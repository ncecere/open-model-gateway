//! OpenRouter speech endpoints (`https://openrouter.ai/api/v1`):
//! - `POST /audio/transcriptions` as JSON `{model, input_audio:{data: raw
//!   base64, format}, response_format:"json", language?, temperature?,
//!   provider}`. OpenRouter ignores `prompt`, so a request with a prompt is
//!   `unsupported_capability` rather than silently dropped. Audio requires an
//!   account balance of at least $0.50: the upstream 402 is a provider
//!   configuration error (the body, which names the account, is never read).
//!   `usage.seconds` (exact decimal text) is the billed duration and
//!   `usage.cost` is provider-cost evidence only.
//! - `POST /audio/speech` `{model, input, voice, response_format: mp3|pcm,
//!   speed?, provider}` returns raw audio with no usage or cost, so the
//!   gateway values it locally from the exact character count.
use serde_json::json;

use super::{OpenRouterAdapter, embedded_error, parse, read, transport};
use crate::{
    inference::{error::InferenceError, types::*},
    providers::audio::{decode_transcription, media_type, read_json, speech_body},
};

type Result<T> = std::result::Result<T, InferenceError>;

pub(super) fn supports_transcription(request: &TranscriptionRequest) -> bool {
    request.prompt.is_none()
}
pub(super) fn supports_speech(request: &SpeechRequest) -> bool {
    matches!(
        request.response_format,
        SpeechFormat::Mp3 | SpeechFormat::Pcm
    )
}
fn wire_format(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Flac => "flac",
        AudioFormat::Mp3 => "mp3",
        AudioFormat::Mp4 => "m4a",
        AudioFormat::Ogg => "ogg",
        AudioFormat::Wav => "wav",
        AudioFormat::Webm => "webm",
    }
}
/// Standard base64 with padding (raw data, not a data URI).
pub(super) fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl OpenRouterAdapter {
    pub(super) async fn transcribe(
        &self,
        target: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse> {
        request.validate()?;
        if !supports_transcription(&request) {
            return Err(InferenceError::Unsupported);
        }
        let mut body = json!({
            "model": target.upstream_model,
            "input_audio": {"data": base64(&request.audio), "format": wire_format(request.format)},
            "response_format": "json",
            "provider": self.provider_preferences(),
        });
        if let Some(language) = &request.language {
            body["language"] = json!(language);
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        let response = self.post(target, "/audio/transcriptions", &body).await?;
        let bytes = read_json(response, transport).await?;
        let (value, cost) = parse(&bytes)?;
        let mut decoded = decode_transcription(&bytes, &value, &request)?;
        decoded.usage.provider_cost_microusd = cost;
        Ok(decoded)
    }

    pub(super) async fn speak(
        &self,
        target: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse> {
        request.validate()?;
        if !supports_speech(&request) {
            return Err(InferenceError::Unsupported);
        }
        let mut body = json!({
            "model": target.upstream_model,
            "input": request.input,
            "voice": request.voice,
            "response_format": request.response_format.as_str(),
            "provider": self.provider_preferences(),
        });
        if let Some(speed) = request.speed {
            body["speed"] = json!(speed);
        }
        let response = self.post(target, "/audio/speech", &body).await?;
        if media_type(&response).as_deref() == Some("application/json") {
            // A 200 JSON body on speech is an embedded error (mapped by code)
            // or an unexpected envelope; never audio.
            let bytes = read(response).await?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| InferenceError::InvalidUpstream)?;
            embedded_error(&value)?;
            return Err(InferenceError::InvalidUpstream);
        }
        speech_body(response, &request, transport).await
    }
}

#[cfg(test)]
mod tests;
