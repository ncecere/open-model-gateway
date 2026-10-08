//! Opt-in, paid-capable speech acceptance through the real adapter code.
//! Never runs in CI: `#[ignore]`, plus `AUDIO_LIVE=1` and the server-held keys
//! `OPENAI_KEY` / `OPEN_ROUTER`. Prints only outcomes, byte counts and usage
//! counters: never keys, audio or transcript text. Nothing is written to disk.
//! Expected spend: well under $0.001 (1 s whisper-1, two 11-character TTS
//! requests; OpenRouter STT is rejected with 402 before any provider work on
//! an account without credit).
use std::sync::Arc;

use futures_util::StreamExt;
use open_model_gateway::{
    inference::{error::InferenceError, types::*},
    providers::{
        ProviderAdapter,
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::EnvSecrets,
    },
};

fn target(provider: &str, model: &str, key: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: provider.into(),
        upstream_model: model.into(),
        credential_ref: format!("env:{key}"),
        endpoint: None,
        region: None,
        supported_protocols: vec![],
    }
}
/// ~1 s, 16 kHz mono 16-bit, quiet 440 Hz tone.
fn tone_wav() -> Vec<u8> {
    let samples: Vec<i16> = (0..16_000)
        .map(|i| ((i as f64 * 440.0 * std::f64::consts::TAU / 16_000.0).sin() * 3000.0) as i16)
        .collect();
    let data = (samples.len() * 2) as u32;
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for (v, w) in [
        (16u32, 4),
        (1, 2),
        (1, 2),
        (16_000, 4),
        (32_000, 4),
        (2, 2),
        (16, 2),
    ] {
        b.extend_from_slice(&v.to_le_bytes()[..w]);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}
fn transcription() -> TranscriptionRequest {
    TranscriptionRequest::new(
        "live/stt".into(),
        Arc::from(tone_wav()),
        AudioFormat::Wav,
        None,
        None,
        None,
        TranscriptionFormat::Json,
    )
}
fn speech(voice: &str) -> SpeechRequest {
    SpeechRequest {
        model: "live/tts".into(),
        input: "Hello there".into(),
        voice: voice.into(),
        response_format: SpeechFormat::Mp3,
        speed: None,
    }
}
fn report(name: &str, usage: Usage) {
    let m = usage.meters.unwrap_or_default();
    eprintln!(
        "LIVE {name}: ok input_tokens={:?} output_tokens={:?} input_audio_ms={:?} input_characters={:?} output_audio_ms={:?} requests={:?} provider_cost_microusd={:?}",
        usage.input_tokens,
        usage.output_tokens,
        m.input_audio_seconds_ms,
        m.input_characters,
        m.output_audio_seconds_ms,
        m.requests,
        usage.provider_cost_microusd,
    );
}
async fn audio_bytes(response: SpeechResponse) -> Result<(usize, bool), InferenceError> {
    let mut audio = response.audio;
    let mut total = 0;
    let mut first = None;
    while let Some(chunk) = audio.next().await {
        let chunk = chunk?;
        if first.is_none() && chunk.len() >= 2 {
            first = Some([chunk[0], chunk[1]]);
        }
        total += chunk.len();
    }
    // MP3: ID3 tag or MPEG frame sync; only the check result is printed.
    let mp3 = first.is_some_and(|h| &h == b"ID" || (h[0] == 0xFF && h[1] & 0xE0 == 0xE0));
    Ok((total, mp3))
}

#[tokio::test]
#[ignore = "live speech requests: set AUDIO_LIVE=1, OPENAI_KEY and OPEN_ROUTER"]
async fn audio_live_openai_and_openrouter() {
    assert_eq!(
        std::env::var("AUDIO_LIVE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let only = std::env::var("AUDIO_LIVE_ONLY").ok();
    let run = |name: &str| {
        only.as_deref()
            .is_none_or(|o| o.split(',').any(|s| s == name))
    };
    let mut failures = Vec::new();
    let openai = OpenAiAdapter::new(Arc::new(EnvSecrets::new(["OPENAI_KEY".to_owned()]))).unwrap();
    let openrouter = OpenRouterAdapter::new(
        Arc::new(EnvSecrets::new(["OPEN_ROUTER".to_owned()])),
        OpenRouterConfig::default(),
    )
    .unwrap();

    if run("openai_stt") {
        let request = transcription();
        eprintln!(
            "LIVE whisper-1 measured_duration_ms={:?}",
            request.duration_ms
        );
        match openai
            .execute_audio_transcription(&target("openai", "whisper-1", "OPENAI_KEY"), request)
            .await
        {
            Ok(r) => {
                eprintln!("LIVE whisper-1 transcript_bytes={}", r.text.len());
                assert!(r.usage.meters.unwrap().input_audio_seconds_ms.is_some());
                report("openai whisper-1", r.usage);
            }
            Err(e) => failures.push(format!("openai_stt: {}", e.code())),
        }
    }
    if run("openai_tts") {
        match openai
            .execute_audio_speech(&target("openai", "tts-1", "OPENAI_KEY"), speech("alloy"))
            .await
        {
            Ok(r) => {
                let usage = r.usage;
                eprintln!("LIVE tts-1 content_type={}", r.content_type);
                match audio_bytes(r).await {
                    Ok((bytes, mp3)) => {
                        eprintln!("LIVE tts-1 audio_bytes={bytes} mp3_magic={mp3}");
                        assert!(bytes > 0 && mp3);
                        report("openai tts-1", usage);
                    }
                    Err(e) => failures.push(format!("openai_tts body: {}", e.code())),
                }
            }
            Err(e) => failures.push(format!("openai_tts: {}", e.code())),
        }
    }
    if run("openrouter_tts") {
        match openrouter
            .execute_audio_speech(
                &target("openrouter", "microsoft/mai-voice-2-flash", "OPEN_ROUTER"),
                speech("en-US-Harper:MAI-Voice-2"),
            )
            .await
        {
            Ok(r) => {
                let usage = r.usage;
                eprintln!("LIVE mai-voice-2-flash content_type={}", r.content_type);
                match audio_bytes(r).await {
                    Ok((bytes, mp3)) => {
                        eprintln!("LIVE mai-voice-2-flash audio_bytes={bytes} mp3_magic={mp3}");
                        assert!(bytes > 0 && mp3);
                        report("openrouter microsoft/mai-voice-2-flash", usage);
                    }
                    Err(e) => failures.push(format!("openrouter_tts body: {}", e.code())),
                }
            }
            Err(e) => failures.push(format!("openrouter_tts: {}", e.code())),
        }
    }
    if run("openrouter_stt") {
        // Without ≥ $0.50 balance OpenRouter answers 402 before any provider
        // work; the gateway maps it to a provider configuration error.
        match openrouter
            .execute_audio_transcription(
                &target("openrouter", "openai/whisper-large-v3-turbo", "OPEN_ROUTER"),
                transcription(),
            )
            .await
        {
            Err(e) => {
                eprintln!(
                    "LIVE openrouter stt: {} (expected provider_configuration_error)",
                    e.code()
                );
                if e != InferenceError::Configuration {
                    failures.push(format!("openrouter_stt mapping: {}", e.code()));
                }
            }
            Ok(r) => report(
                "openrouter whisper-large-v3-turbo (account has credit)",
                r.usage,
            ),
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}
