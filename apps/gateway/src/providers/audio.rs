//! Shared speech wire helpers for native adapters: multipart encoding,
//! transcription response/usage normalization, and bounded speech passthrough.
//! Nothing here logs or retains audio, transcripts or input text.
use axum::body::Bytes;
use futures_util::StreamExt;
use reqwest::header;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

use super::metering;
use crate::inference::{
    audio::{
        SpeechRequest, SpeechResponse, TRANSCRIPTION_MAX_TEXT_BYTES, TranscriptionRequest,
        TranscriptionResponse, transcription_meters,
    },
    error::InferenceError,
    evidence,
    types::Usage,
};

type Result<T> = std::result::Result<T, InferenceError>;

/// `multipart/form-data` built by the gateway. Field names are fixed by the
/// adapter; the client filename is never forwarded (`audio.<ext>` instead).
pub(crate) struct Multipart {
    boundary: String,
    body: Vec<u8>,
}
impl Multipart {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            boundary: format!("omg-{}", uuid::Uuid::new_v4().simple()),
            body: Vec::with_capacity(capacity + 1024),
        }
    }
    fn head(&mut self, disposition: &str, content_type: Option<&str>) {
        self.body.extend_from_slice(b"--");
        self.body.extend_from_slice(self.boundary.as_bytes());
        self.body.extend_from_slice(
            format!("\r\nContent-Disposition: form-data; {disposition}\r\n").as_bytes(),
        );
        if let Some(t) = content_type {
            self.body
                .extend_from_slice(format!("Content-Type: {t}\r\n").as_bytes());
        }
        self.body.extend_from_slice(b"\r\n");
    }
    pub(crate) fn text(&mut self, name: &'static str, value: &str) {
        self.head(&format!("name=\"{name}\""), None);
        self.body.extend_from_slice(value.as_bytes());
        self.body.extend_from_slice(b"\r\n");
    }
    pub(crate) fn file(&mut self, extension: &'static str, mime: &'static str, data: &[u8]) {
        self.head(
            &format!("name=\"file\"; filename=\"audio.{extension}\""),
            Some(mime),
        );
        self.body.extend_from_slice(data);
        self.body.extend_from_slice(b"\r\n");
    }
    /// `(content type, body)`.
    pub(crate) fn finish(mut self) -> (String, Vec<u8>) {
        self.body.extend_from_slice(b"--");
        self.body.extend_from_slice(self.boundary.as_bytes());
        self.body.extend_from_slice(b"--\r\n");
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

/// Media type without parameters, lowercase.
pub(crate) fn media_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(header::CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Bounded JSON transcription body (4 MiB text cap plus envelope).
pub(crate) async fn read_json(
    response: reqwest::Response,
    transport: fn(reqwest::Error) -> InferenceError,
) -> Result<Vec<u8>> {
    const LIMIT: usize = TRANSCRIPTION_MAX_TEXT_BYTES + 64 * 1024;
    if media_type(&response).as_deref() != Some("application/json")
        || response.content_length().is_some_and(|n| n > LIMIT as u64)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(transport)?;
        if chunk.len() > LIMIT - body.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Seconds as exact JSON number text → milliseconds, rounded up.
pub(crate) fn seconds_text_to_ms(text: &str) -> Result<u64> {
    let micro = metering::usd_text_to_microusd_ceil(text)?;
    u64::try_from(micro)
        .map(|n| n.div_ceil(1000))
        .map_err(|_| InferenceError::InvalidUpstream)
}

#[derive(Deserialize)]
struct SecondsProbe<'a> {
    #[serde(borrow, default)]
    usage: Option<SecondsUsage<'a>>,
}
#[derive(Deserialize)]
struct SecondsUsage<'a> {
    #[serde(borrow, default)]
    seconds: Option<&'a RawValue>,
}

/// Normalize a transcription usage object (OpenAI discriminated
/// `{type:"duration",seconds}` / `{type:"tokens",input_tokens,output_tokens}`
/// or OpenRouter's untyped `{seconds?, input_tokens?, output_tokens?}`).
///
/// - Reported seconds are the billed audio duration.
/// - Token-billed responses (tokens but no seconds) record the server-measured
///   duration (`None` when unknown) as the audio observation.
/// - Unreported counters stay unknown; nothing is fabricated as zero.
fn transcription_usage(
    value: &Value,
    seconds: Option<&str>,
    measured_ms: Option<u64>,
) -> Result<Usage> {
    if value.is_null() {
        return Ok(Usage {
            meters: Some(transcription_meters(None)),
            ..Usage::default()
        });
    }
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    let input = metering::count(&value["input_tokens"])?;
    let output = metering::count(&value["output_tokens"])?;
    let total = metering::count(&value["total_tokens"])?;
    if let (Some(i), Some(o), Some(t)) = (input, output, total)
        && i.checked_add(o) != Some(t)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let audio_ms = seconds.map(seconds_text_to_ms).transpose()?;
    match value["type"].as_str() {
        None if value["type"].is_null() => {}
        Some("duration") if audio_ms.is_some() => {}
        Some("tokens") if input.is_some() && output.is_some() && audio_ms.is_none() => {}
        _ => return Err(InferenceError::InvalidUpstream),
    }
    let mut usage = if input.is_some() || output.is_some() {
        let mut u = metering::input_only(input);
        u.output_tokens = output;
        u
    } else {
        Usage::default()
    };
    let token_billed = input.is_some() || output.is_some();
    usage.meters = Some(transcription_meters(audio_ms.or(if token_billed {
        measured_ms
    } else {
        None
    })));
    Ok(usage)
}

/// Decode a JSON transcription body (`response_format:"json"` upstream).
pub(crate) fn decode_transcription(
    bytes: &[u8],
    value: &Value,
    request: &TranscriptionRequest,
) -> Result<TranscriptionResponse> {
    let probe: SecondsProbe =
        serde_json::from_slice(bytes).map_err(|_| InferenceError::InvalidUpstream)?;
    let seconds = probe.usage.and_then(|u| u.seconds).map(RawValue::get);
    let usage = || transcription_usage(&value["usage"], seconds, request.duration_ms);
    let shape = (|| {
        let object = value.as_object().ok_or(InferenceError::InvalidUpstream)?;
        // Metadata may appear; content-bearing extras (segments, words,
        // logprobs) were not requested and must not be silently dropped.
        for (key, v) in object {
            let tolerated = matches!(
                key.as_str(),
                "text" | "usage" | "id" | "object" | "model" | "provider" | "created"
            );
            let empty = v.is_null() || v.as_array().is_some_and(Vec::is_empty);
            if !tolerated && !empty {
                return Err(InferenceError::InvalidUpstream);
            }
        }
        let text = value["text"]
            .as_str()
            .filter(|t| t.len() <= TRANSCRIPTION_MAX_TEXT_BYTES)
            .ok_or(InferenceError::InvalidUpstream)?
            .to_owned();
        Ok(TranscriptionResponse {
            text,
            usage: usage()?,
        })
    })();
    evidence::preserve(shape, || value["usage"].is_object().then(usage))
}

/// Pass a successful speech body through. The upstream media type must carry
/// the requested format; the first non-empty chunk is awaited so empty or
/// immediately failing bodies fail before any byte reaches the client.
pub(crate) async fn speech_body(
    response: reqwest::Response,
    request: &SpeechRequest,
    transport: fn(reqwest::Error) -> InferenceError,
) -> Result<SpeechResponse> {
    if !media_type(&response).is_some_and(|m| request.response_format.accepts(&m)) {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut chunks = response.bytes_stream();
    let first: Bytes = loop {
        match chunks.next().await {
            Some(Ok(chunk)) if chunk.is_empty() => continue,
            Some(Ok(chunk)) => break chunk,
            Some(Err(error)) => return Err(transport(error)),
            None => return Err(InferenceError::InvalidUpstream),
        }
    };
    let rest = chunks.map(move |chunk| chunk.map_err(transport));
    Ok(SpeechResponse {
        audio: Box::pin(futures_util::stream::once(async move { Ok(first) }).chain(rest)),
        content_type: request.response_format.content_type(),
        usage: request.usage(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::audio::AudioFormat;
    use serde_json::json;
    use std::sync::Arc;

    fn request(duration_ms: Option<u64>) -> TranscriptionRequest {
        TranscriptionRequest {
            model: "m".into(),
            audio: Arc::from(&b"RIFF"[..]),
            format: AudioFormat::Wav,
            language: None,
            prompt: None,
            temperature: None,
            response_format: crate::inference::audio::TranscriptionFormat::Json,
            duration_ms,
        }
    }
    fn decode(v: Value, duration: Option<u64>) -> Result<TranscriptionResponse> {
        let bytes = serde_json::to_vec(&v).unwrap();
        decode_transcription(&bytes, &v, &request(duration))
    }

    #[test]
    fn multipart_is_well_formed_and_hides_client_filenames() {
        let mut m = Multipart::new(4);
        m.text("model", "whisper-1");
        m.file("wav", "audio/wav", b"RIFF");
        let (content_type, body) = m.finish();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        assert!(text.contains(
            "name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n"
        ));
        assert!(text.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
    }

    #[test]
    fn transcription_usage_shapes() {
        // OpenAI whisper-1: billed whole seconds, tokens unknown.
        let r = decode(
            json!({"text":"x","usage":{"type":"duration","seconds":1}}),
            Some(850),
        )
        .unwrap();
        assert_eq!(r.usage.meters.unwrap().input_audio_seconds_ms, Some(1000));
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (None, None));
        assert_eq!(r.usage.meters.unwrap().requests, Some(1));
        // OpenAI gpt-4o-mini-transcribe: tokens + measured duration.
        let r = decode(
            json!({"text":"x","usage":{"type":"tokens","total_tokens":14,"input_tokens":8,"input_token_details":{"text_tokens":0,"audio_tokens":8},"output_tokens":6}}),
            Some(850),
        )
        .unwrap();
        assert_eq!(
            (r.usage.input_tokens, r.usage.output_tokens),
            (Some(8), Some(6))
        );
        assert_eq!(r.usage.billing.unwrap().total_input_tokens, Some(8));
        assert_eq!(r.usage.meters.unwrap().input_audio_seconds_ms, Some(850));
        // OpenRouter: exact decimal seconds, rounded up to ms.
        let r = decode(
            json!({"text":"x","usage":{"seconds":9.2001,"total_tokens":113,"input_tokens":83,"output_tokens":30,"cost":0.000508}}),
            None,
        )
        .unwrap();
        assert_eq!(r.usage.meters.unwrap().input_audio_seconds_ms, Some(9201));
        // No usage: unknown audio, never zero.
        let r = decode(json!({"text":""}), Some(1000)).unwrap();
        assert_eq!(r.usage.meters.unwrap().input_audio_seconds_ms, None);
        assert_eq!(r.usage.input_tokens, None);
        assert_eq!(seconds_text_to_ms("0.85").unwrap(), 850);
        assert_eq!(seconds_text_to_ms("1e0").unwrap(), 1000);
        assert_eq!(seconds_text_to_ms("0.0001").unwrap(), 1);
        assert!(seconds_text_to_ms("-1").is_err());
    }

    #[tokio::test]
    async fn invalid_transcription_bodies_fail_and_keep_valid_usage() {
        for bad in [
            json!({"usage":{"type":"duration","seconds":1}}),
            json!({"text":1}),
            json!({"text":"x","segments":[{"text":"leak"}]}),
            json!({"text":"x","usage":{"type":"duration"}}),
            json!({"text":"x","usage":{"type":"tokens","input_tokens":1}}),
            json!({"text":"x","usage":{"type":"bogus","seconds":1}}),
            json!({"text":"x","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":3}}),
            json!({"text":"x","usage":{"seconds":-1}}),
            json!({"text":"x","usage":"1s"}),
            json!(["x"]),
        ] {
            assert!(decode(bad.clone(), Some(1000)).is_err(), "{bad}");
        }
        let (result, kept) = crate::inference::evidence::capture(async {
            decode(json!({"usage":{"type":"duration","seconds":2}}), None)
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            kept.unwrap().meters.unwrap().input_audio_seconds_ms,
            Some(2000)
        );
    }
}
