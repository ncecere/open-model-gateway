//! Speech workloads: `audio_transcriptions` (speech-to-text) and
//! `audio_speech` (text-to-speech), first increment (no realtime).
//!
//! Metering (see `docs/protocol-matrix.md`):
//! - **Transcription** meters `input_audio_seconds_ms`. Providers that bill by
//!   duration report it (OpenAI `usage.type:"duration"` whole seconds,
//!   OpenRouter `usage.seconds`); token-billed models report tokens and the
//!   gateway records the server-measured duration when it could determine it.
//!   Admission bounds the meter with the measured duration (rounded up to whole
//!   seconds plus one second of decoder/rounding slack); when the container
//!   cannot be measured (MP4/M4A/WebM, or a malformed header) the pinned
//!   price's `max_units` is the only bound, otherwise the meter is unbounded and
//!   budgeted admission is refused (Phase 1 rule).
//! - **Speech** meters `input_characters` exactly, counted by the gateway as
//!   Unicode scalar values (`char`s) of `input` before admission. Output audio
//!   duration is unknown (never reported), so prices must mark
//!   `output_audio_seconds_ms` not applicable or bound it with `max_units`.
//!
//! Upload bytes, transcripts, speech input and generated audio are never
//! logged or stored.
use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::time::timeout_at;
use uuid::Uuid;

use super::{
    Engine,
    deadline_stream::DeadlineStream,
    error::InferenceError,
    types::{ApiProtocol, Deployment, Usage, WorkloadKind},
    workload::{OutputReservation, StreamSettlement, Workload, WorkloadAdmission, within_ceilings},
};
use crate::{auth::Principal, billing::MeterUsage, providers::ProviderAdapter};

pub mod duration;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod tests_support {
    /// PCM WAV: 16 kHz mono 16-bit, `ms` of silence.
    pub fn wav(ms: u64) -> Vec<u8> {
        let data = (16_000 * 2 * ms / 1000) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        for (value, width) in [
            (16u32, 4),
            (1, 2),
            (1, 2),
            (16_000, 4),
            (32_000, 4),
            (2, 2),
            (16, 2),
        ] {
            b.extend_from_slice(&value.to_le_bytes()[..width]);
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        b.resize(b.len() + data as usize, 0);
        b
    }
}

type Result<T> = std::result::Result<T, InferenceError>;

/// Generated audio bytes, still streaming from upstream.
pub type AudioStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// Absolute ceilings independent of configuration.
pub const HARD_MAX_UPLOAD_BYTES: usize = 64 * MIB;
pub const SPEECH_HARD_MAX_INPUT_CHARS: usize = 100_000;
pub const TRANSCRIPTION_MAX_PROMPT_BYTES: usize = 4096;
pub const TRANSCRIPTION_MAX_TEXT_BYTES: usize = 4 * MIB;
pub const SPEECH_MAX_VOICE_BYTES: usize = 64;

/// Configurable speech limits (separate from the JSON/multipart body caps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioLimits {
    /// Maximum decoded `file` part size for transcriptions.
    pub max_upload_bytes: usize,
    /// Maximum speech `input` length in Unicode scalar values.
    pub max_speech_input_chars: usize,
    /// Maximum generated audio bytes streamed to a client.
    pub max_speech_output_bytes: u64,
}
impl Default for AudioLimits {
    fn default() -> Self {
        Self {
            max_upload_bytes: 25 * MIB,
            max_speech_input_chars: 4096,
            max_speech_output_bytes: 64 * MIB as u64,
        }
    }
}
impl AudioLimits {
    pub fn validate(self) -> bool {
        (KIB..=HARD_MAX_UPLOAD_BYTES).contains(&self.max_upload_bytes)
            && (1..=SPEECH_HARD_MAX_INPUT_CHARS).contains(&self.max_speech_input_chars)
            && (64 * KIB as u64..=1024 * MIB as u64).contains(&self.max_speech_output_bytes)
    }
    /// `GATEWAY_AUDIO_MAX_UPLOAD_BYTES`, `GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS`,
    /// `GATEWAY_AUDIO_SPEECH_MAX_OUTPUT_BYTES`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut limits = Self::default();
        let parse = |name: &str| -> anyhow::Result<Option<u64>> {
            get(name)
                .map(|v| {
                    v.trim()
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("{name} must be an integer"))
                })
                .transpose()
        };
        if let Some(n) = parse("GATEWAY_AUDIO_MAX_UPLOAD_BYTES")? {
            limits.max_upload_bytes = usize::try_from(n).unwrap_or(usize::MAX);
        }
        if let Some(n) = parse("GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS")? {
            limits.max_speech_input_chars = usize::try_from(n).unwrap_or(usize::MAX);
        }
        if let Some(n) = parse("GATEWAY_AUDIO_SPEECH_MAX_OUTPUT_BYTES")? {
            limits.max_speech_output_bytes = n;
        }
        anyhow::ensure!(limits.validate(), "audio limits out of range");
        Ok(limits)
    }
}

fn valid_model(model: &str) -> bool {
    !model.trim().is_empty() && model.len() <= 200
}

// --------------------------------------------------------- Transcription ----

/// Accepted upload containers (OpenAI's documented set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    Flac,
    Mp3,
    Mp4,
    Ogg,
    Wav,
    Webm,
}
impl AudioFormat {
    pub fn from_extension(extension: &str) -> Option<Self> {
        Some(match extension {
            "flac" => Self::Flac,
            "mp3" | "mpeg" | "mpga" => Self::Mp3,
            "mp4" | "m4a" => Self::Mp4,
            "ogg" => Self::Ogg,
            "wav" => Self::Wav,
            "webm" => Self::Webm,
            _ => return None,
        })
    }
    /// `Some(None)`: a generic type that defers to the extension.
    /// `None`: not an accepted upload type.
    pub fn from_content_type(content_type: &str) -> Option<Option<Self>> {
        Some(Some(match content_type {
            "application/octet-stream" => return Some(None),
            "audio/flac" | "audio/x-flac" => Self::Flac,
            "audio/mpeg" | "audio/mp3" | "audio/mpga" => Self::Mp3,
            "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "video/mp4" => Self::Mp4,
            "audio/ogg" | "application/ogg" => Self::Ogg,
            "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => Self::Wav,
            "audio/webm" | "video/webm" => Self::Webm,
            _ => return None,
        }))
    }
    /// Upstream filename extension (the client filename is never forwarded).
    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Mp4 => "mp4",
            Self::Ogg => "ogg",
            Self::Wav => "wav",
            Self::Webm => "webm",
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            Self::Flac => "audio/flac",
            Self::Mp3 => "audio/mpeg",
            Self::Mp4 => "audio/mp4",
            Self::Ogg => "audio/ogg",
            Self::Wav => "audio/wav",
            Self::Webm => "audio/webm",
        }
    }
    /// Container magic must match the declared format.
    pub fn sniff(self, bytes: &[u8]) -> bool {
        let at =
            |offset: usize, magic: &[u8]| bytes.get(offset..offset + magic.len()) == Some(magic);
        match self {
            Self::Wav => at(0, b"RIFF") && at(8, b"WAVE"),
            Self::Flac => at(duration::id3_len(bytes).unwrap_or(0), b"fLaC"),
            Self::Mp3 => {
                at(0, b"ID3")
                    || bytes
                        .get(..2)
                        .is_some_and(|h| h[0] == 0xFF && h[1] & 0xE0 == 0xE0)
            }
            Self::Ogg => at(0, b"OggS"),
            Self::Mp4 => at(4, b"ftyp"),
            Self::Webm => at(0, &[0x1A, 0x45, 0xDF, 0xA3]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptionFormat {
    Json,
    Text,
}

/// No `Debug`: carries the upload and the client prompt.
#[derive(Clone)]
pub struct TranscriptionRequest {
    pub model: String,
    /// Shared so failover clones never copy the upload.
    pub audio: Arc<[u8]>,
    pub format: AudioFormat,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub temperature: Option<f64>,
    pub response_format: TranscriptionFormat,
    /// Server-measured duration in ms (rounded up); `None` if undeterminable.
    pub duration_ms: Option<u64>,
}
impl TranscriptionRequest {
    /// Build a request and measure its duration from the container header.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: String,
        audio: Arc<[u8]>,
        format: AudioFormat,
        language: Option<String>,
        prompt: Option<String>,
        temperature: Option<f64>,
        response_format: TranscriptionFormat,
    ) -> Self {
        let duration_ms = duration::measure(format, &audio);
        Self {
            model,
            audio,
            format,
            language,
            prompt,
            temperature,
            response_format,
            duration_ms,
        }
    }
    pub fn validate(&self) -> Result<()> {
        let language_ok = self.language.as_deref().is_none_or(|l| {
            (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase())
        });
        let prompt_ok = self.prompt.as_deref().is_none_or(|p| {
            !p.trim().is_empty()
                && p.len() <= TRANSCRIPTION_MAX_PROMPT_BYTES
                && !p
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        });
        if !valid_model(&self.model)
            || self.audio.is_empty()
            || self.audio.len() > HARD_MAX_UPLOAD_BYTES
            || !self.format.sniff(&self.audio)
            || !language_ok
            || !prompt_ok
            || self
                .temperature
                .is_some_and(|t| !t.is_finite() || !(0.0..=1.0).contains(&t))
        {
            return Err(InferenceError::InvalidRequest);
        }
        Ok(())
    }
    /// Admission ceiling for `input_audio_seconds_ms`: the measured duration
    /// rounded up to whole seconds (providers round up) plus one second.
    pub fn audio_ceiling_ms(&self) -> Option<u64> {
        self.duration_ms
            .map(|d| (d.div_ceil(1000).saturating_add(1)).saturating_mul(1000))
    }
}
pub struct TranscriptionResponse {
    pub text: String,
    pub usage: Usage,
}

/// Meters for one transcription request. Meters the workload cannot produce
/// are semantic zeros; audio is the billable/observed duration (or unknown).
pub fn transcription_meters(input_audio_ms: Option<u64>) -> MeterUsage {
    MeterUsage {
        output_images: Some(0),
        input_characters: Some(0),
        input_audio_seconds_ms: input_audio_ms,
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some(1),
        output_video_seconds_ms: None,
    }
}

#[async_trait]
impl Workload for TranscriptionRequest {
    type Response = TranscriptionResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::AudioTranscriptions;
    fn model(&self) -> &str {
        &self.model
    }
    fn validate(&self) -> Result<()> {
        TranscriptionRequest::validate(self)
    }
    fn admission(&self) -> WorkloadAdmission {
        WorkloadAdmission {
            kind: WorkloadKind::AudioTranscriptions,
            // Token-billed transcription models report output tokens the
            // request cannot bound: reserve the price's trusted ceiling.
            output: OutputReservation::PriceCeiling,
            unit_ceilings: MeterUsage {
                input_audio_seconds_ms: self.audio_ceiling_ms(),
                ..transcription_meters(None)
            },
        }
    }
    fn supported_by(&self, adapter: &dyn ProviderAdapter, target: &Deployment) -> bool {
        adapter.supports_transcription_request(target, self)
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<TranscriptionResponse> {
        adapter.execute_audio_transcription(target, self).await
    }
    fn usage(response: &TranscriptionResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &TranscriptionResponse) -> bool {
        response.text.len() <= TRANSCRIPTION_MAX_TEXT_BYTES
            && response.usage.meters.is_some_and(|m| m.requests == Some(1))
            && within_ceilings(response.usage, self.admission().unit_ceilings)
    }
}

// ---------------------------------------------------------------- Speech ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechFormat {
    Mp3,
    Wav,
    Opus,
    Pcm,
}
impl SpeechFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mp3" => Self::Mp3,
            "wav" => Self::Wav,
            "opus" => Self::Opus,
            "pcm" => Self::Pcm,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Opus => "opus",
            Self::Pcm => "pcm",
        }
    }
    /// Gateway response `Content-Type` (explicit, never copied from upstream).
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::Wav => "audio/wav",
            Self::Opus => "audio/opus",
            Self::Pcm => "audio/pcm",
        }
    }
    /// Upstream media types (parameters stripped, lowercase) that carry this
    /// format. Anything else, including JSON on a 200, is invalid.
    pub fn accepts(self, upstream: &str) -> bool {
        match self {
            Self::Mp3 => matches!(upstream, "audio/mpeg" | "audio/mp3"),
            Self::Wav => matches!(
                upstream,
                "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave"
            ),
            Self::Opus => matches!(upstream, "audio/opus" | "audio/ogg"),
            Self::Pcm => matches!(
                upstream,
                "audio/pcm" | "audio/l16" | "application/octet-stream"
            ),
        }
    }
}

/// No `Debug`: carries the text to synthesize.
#[derive(Clone)]
pub struct SpeechRequest {
    pub model: String,
    pub input: String,
    pub voice: String,
    pub response_format: SpeechFormat,
    pub speed: Option<f64>,
}
impl SpeechRequest {
    /// Exact billable character count: Unicode scalar values.
    pub fn input_characters(&self) -> u64 {
        self.input.chars().count() as u64
    }
    pub fn validate(&self) -> Result<()> {
        let voice_ok = !self.voice.is_empty()
            && self.voice.len() <= SPEECH_MAX_VOICE_BYTES
            && self
                .voice
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
        if !valid_model(&self.model)
            || self.input.trim().is_empty()
            || self.input_characters() > SPEECH_HARD_MAX_INPUT_CHARS as u64
            || !voice_ok
            || self
                .speed
                .is_some_and(|s| !s.is_finite() || !(0.25..=4.0).contains(&s))
        {
            return Err(InferenceError::InvalidRequest);
        }
        Ok(())
    }
    /// The usage every speech attempt reports: exact characters, one request,
    /// semantic zeros, and unknown output audio duration.
    pub fn usage(&self) -> Usage {
        Usage {
            meters: Some(MeterUsage {
                output_images: Some(0),
                input_characters: Some(self.input_characters()),
                input_audio_seconds_ms: Some(0),
                output_audio_seconds_ms: None,
                search_units: Some(0),
                requests: Some(1),
                output_video_seconds_ms: None,
            }),
            ..Usage::default()
        }
    }
}
pub struct SpeechResponse {
    pub audio: AudioStream,
    pub content_type: &'static str,
    pub usage: Usage,
}

#[async_trait]
impl Workload for SpeechRequest {
    type Response = SpeechResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::AudioSpeech;
    const STREAMED: bool = true;
    fn model(&self) -> &str {
        &self.model
    }
    fn validate(&self) -> Result<()> {
        SpeechRequest::validate(self)
    }
    fn admission(&self) -> WorkloadAdmission {
        let mut ceilings = self.usage().meters.unwrap_or_default();
        ceilings.output_audio_seconds_ms = None;
        WorkloadAdmission {
            kind: WorkloadKind::AudioSpeech,
            // Token-priced TTS may report output (audio) tokens.
            output: OutputReservation::PriceCeiling,
            unit_ceilings: ceilings,
        }
    }
    fn supported_by(&self, adapter: &dyn ProviderAdapter, target: &Deployment) -> bool {
        adapter.supports_speech_request(target, self)
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<SpeechResponse> {
        adapter.execute_audio_speech(target, self).await
    }
    fn usage(response: &SpeechResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &SpeechResponse) -> bool {
        response.content_type == self.response_format.content_type()
            && response.usage.meters.is_some_and(|m| {
                m.input_characters == Some(self.input_characters()) && m.requests == Some(1)
            })
            && within_ceilings(response.usage, self.admission().unit_ceilings)
    }
    fn attach(response: SpeechResponse, settlement: StreamSettlement) -> SpeechResponse {
        let max = settlement.limits().audio.max_speech_output_bytes;
        SpeechResponse {
            audio: track(response.audio, settlement, max),
            ..response
        }
    }
}

/// Pass audio through until upstream EOF, enforcing the output byte bound and
/// the request deadline (even if the client stops polling). Success is
/// durably settled before the body ends cleanly; any failure aborts the body
/// with an error (the client sees a truncated transfer, never a clean end).
/// Dropping the body drops the upstream transport and records a cancellation.
fn track(upstream: AudioStream, settlement: StreamSettlement, max_bytes: u64) -> AudioStream {
    let deadline = settlement.deadline();
    let mut upstream = DeadlineStream::new(upstream, deadline, Some(settlement.permit()));
    Box::pin(async_stream::stream! {
        let mut settlement = settlement;
        let mut total: u64 = 0;
        loop {
            let next = match timeout_at(deadline, upstream.next()).await {
                Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
                Ok(Some(Err(error))) => Err(error),
                Ok(None) => Ok(None),
                Err(_) => Err(InferenceError::Timeout),
            };
            match next {
                Ok(Some(chunk)) => {
                    total = total.saturating_add(chunk.len() as u64);
                    if total > max_bytes {
                        yield Err(settlement.fail(InferenceError::InvalidUpstream).await);
                        break;
                    }
                    if !chunk.is_empty() {
                        yield Ok(chunk);
                    }
                }
                Ok(None) if total == 0 => {
                    yield Err(settlement.fail(InferenceError::InvalidUpstream).await);
                    break;
                }
                Ok(None) => {
                    if let Err(error) = settlement.succeed().await {
                        yield Err(error);
                    }
                    break;
                }
                Err(error) => {
                    yield Err(settlement.fail(error).await);
                    break;
                }
            }
        }
    })
}

impl Engine {
    /// Transcribe an upload, enforcing the configured upload cap.
    pub async fn execute_transcription(
        &self,
        principal: Principal,
        request: TranscriptionRequest,
        request_id: Uuid,
    ) -> Result<TranscriptionResponse> {
        if request.audio.len() > self.limits.audio.max_upload_bytes {
            return Err(InferenceError::InvalidRequest);
        }
        self.execute_workload(principal, request, request_id).await
    }
    /// Synthesize speech, enforcing the configured input-character cap. The
    /// returned body settles the attempt when it ends or is dropped.
    pub async fn execute_speech(
        &self,
        principal: Principal,
        request: SpeechRequest,
        request_id: Uuid,
    ) -> Result<SpeechResponse> {
        if request.input_characters() > self.limits.audio.max_speech_input_chars as u64 {
            return Err(InferenceError::InvalidRequest);
        }
        self.execute_workload(principal, request, request_id).await
    }
}
