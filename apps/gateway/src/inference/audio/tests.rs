use super::*;
use crate::{
    inference::{
        EngineLimits,
        repository::{ExecutionFinish, ExecutionStart, InferenceRepository, Outcome},
        types::{Capabilities, ChatRequest, ProviderOutput},
    },
    providers::ProviderRegistry,
};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

// ------------------------------------------------------------- fixtures ----

use super::tests_support::wav;
/// MPEG-1 Layer III, 128 kbps, 44.1 kHz frames (417 bytes, no padding).
fn mp3(frames: usize) -> Vec<u8> {
    let mut b = Vec::new();
    for _ in 0..frames {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        b.extend_from_slice(&f);
    }
    b
}
fn flac(rate: u64, total: u64) -> Vec<u8> {
    let mut b = b"fLaC".to_vec();
    b.extend_from_slice(&[0x80, 0, 0, 34]);
    let mut info = [0u8; 34];
    let packed = (rate << 44) | (1 << 41) | (15 << 36) | total;
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    b.extend_from_slice(&info);
    b.extend_from_slice(&[0xFF, 0xF8, 0, 0]);
    b
}
fn ogg_page(flags: u8, granule: u64, serial: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 255);
    let mut p = b"OggS".to_vec();
    p.push(0);
    p.push(flags);
    p.extend_from_slice(&granule.to_le_bytes());
    p.extend_from_slice(&serial.to_le_bytes());
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&[0; 4]);
    p.push(1);
    p.push(payload.len() as u8);
    p.extend_from_slice(payload);
    let crc = duration::ogg_crc(&p);
    p[22..26].copy_from_slice(&crc.to_le_bytes());
    p
}
fn opus(granule: u64, preskip: u16) -> Vec<u8> {
    let mut head = b"OpusHead\x01\x01".to_vec();
    head.extend_from_slice(&preskip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    [
        ogg_page(0x02, 0, 7, 0, &head),
        ogg_page(0, 0, 7, 1, b"OpusTags\0\0\0\0\0\0\0\0"),
        ogg_page(0x04, granule, 7, 2, &[0xFC; 20]),
    ]
    .concat()
}

#[test]
fn duration_wav_counts_every_pcm_byte() {
    assert_eq!(duration::measure(AudioFormat::Wav, &wav(850)), Some(850));
    assert_eq!(duration::measure(AudioFormat::Wav, &wav(1)), Some(1));
    // Understated data size: trailing bytes still count (upper bound).
    let mut lying = wav(1000);
    let at = lying.len() - 32_000 - 4;
    lying[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(duration::measure(AudioFormat::Wav, &lying), Some(1000));
    // Inconsistent byte rate, compressed tag, or no fmt chunk: unknown.
    let mut bad_rate = wav(500);
    bad_rate[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut adpcm = wav(500);
    adpcm[20..22].copy_from_slice(&2u16.to_le_bytes());
    for bad in [bad_rate, adpcm, wav(500)[..36].to_vec(), b"RIFF".to_vec()] {
        assert_eq!(duration::measure(AudioFormat::Wav, &bad), None);
    }
}

#[test]
fn duration_mp3_walks_all_frames() {
    // 38 frames × 1152 / 44100 = 0.99265 s → 993 ms.
    assert_eq!(duration::measure(AudioFormat::Mp3, &mp3(38)), Some(993));
    let mut tagged = b"ID3\x04\0\0\0\0\0\x05hello".to_vec();
    tagged.extend_from_slice(&mp3(38));
    tagged.extend_from_slice(&[0; 7]);
    let mut v1 = b"TAG".to_vec();
    v1.resize(128, b' ');
    tagged.extend_from_slice(&v1);
    assert_eq!(duration::measure(AudioFormat::Mp3, &tagged), Some(993));
    // Junk between frames could hide frames: unknown.
    let mut junk = mp3(10);
    junk.extend_from_slice(b"junk");
    junk.extend_from_slice(&mp3(10));
    assert_eq!(duration::measure(AudioFormat::Mp3, &junk), None);
    // A truncated final frame still counts.
    assert_eq!(
        duration::measure(AudioFormat::Mp3, &mp3(2)[..500]),
        Some(53)
    );
    assert_eq!(duration::measure(AudioFormat::Mp3, b"ID3"), None);
}

#[test]
fn duration_flac_and_ogg_and_unmeasured_containers() {
    assert_eq!(
        duration::measure(AudioFormat::Flac, &flac(44_100, 88_200)),
        Some(2000)
    );
    assert_eq!(duration::measure(AudioFormat::Flac, &flac(44_100, 0)), None);
    // Opus: 48 kHz granules minus pre-skip.
    assert_eq!(
        duration::measure(AudioFormat::Ogg, &opus(48_312, 312)),
        Some(1000)
    );
    let mut vorbis = b"\x01vorbis\0\0\0\0\x01".to_vec();
    vorbis.extend_from_slice(&16_000u32.to_le_bytes());
    vorbis.resize(30, 0);
    let file = [
        ogg_page(0x02, 0, 1, 0, &vorbis),
        ogg_page(0x04, 8000, 1, 1, &[1; 10]),
    ]
    .concat();
    assert_eq!(duration::measure(AudioFormat::Ogg, &file), Some(500));
    let mut corrupt = opus(48_312, 312);
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    let mut trailing = opus(48_312, 312);
    trailing.push(0);
    let mut two_streams = opus(48_312, 312);
    two_streams.extend_from_slice(&ogg_page(0, 96_000, 8, 0, &[0; 4]));
    for bad in [corrupt, trailing, two_streams] {
        assert_eq!(duration::measure(AudioFormat::Ogg, &bad), None);
    }
    assert_eq!(
        duration::measure(AudioFormat::Mp4, b"\0\0\0\x18ftypM4A "),
        None
    );
    assert_eq!(
        duration::measure(AudioFormat::Webm, &[0x1A, 0x45, 0xDF, 0xA3]),
        None
    );
}

fn transcription(audio: Vec<u8>, format: AudioFormat) -> TranscriptionRequest {
    TranscriptionRequest::new(
        "company/whisper".into(),
        Arc::from(audio),
        format,
        Some("en".into()),
        None,
        Some(0.0),
        TranscriptionFormat::Json,
    )
}
fn speech(input: &str) -> SpeechRequest {
    SpeechRequest {
        model: "company/tts".into(),
        input: input.into(),
        voice: "en-US-Harper:MAI-Voice-2".into(),
        response_format: SpeechFormat::Mp3,
        speed: None,
    }
}

#[test]
fn request_validation_and_ceilings() {
    let t = transcription(wav(850), AudioFormat::Wav);
    assert!(t.validate().is_ok());
    assert_eq!(t.duration_ms, Some(850));
    // Rounded up to whole seconds plus one second of slack.
    assert_eq!(t.audio_ceiling_ms(), Some(2000));
    assert_eq!(
        t.admission().unit_ceilings,
        MeterUsage {
            input_audio_seconds_ms: Some(2000),
            ..transcription_meters(None)
        }
    );
    assert_eq!(t.admission().output, OutputReservation::PriceCeiling);
    for bad in [
        TranscriptionRequest {
            format: AudioFormat::Mp3,
            ..t.clone()
        },
        TranscriptionRequest {
            audio: Arc::from(&[][..]),
            ..t.clone()
        },
        TranscriptionRequest {
            language: Some("English".into()),
            ..t.clone()
        },
        TranscriptionRequest {
            prompt: Some(" ".into()),
            ..t.clone()
        },
        TranscriptionRequest {
            prompt: Some("x".repeat(TRANSCRIPTION_MAX_PROMPT_BYTES + 1)),
            ..t.clone()
        },
        TranscriptionRequest {
            temperature: Some(1.5),
            ..t.clone()
        },
        TranscriptionRequest {
            temperature: Some(f64::NAN),
            ..t.clone()
        },
        TranscriptionRequest {
            model: String::new(),
            ..t.clone()
        },
    ] {
        assert_eq!(bad.validate(), Err(InferenceError::InvalidRequest));
    }
    let unmeasured = transcription(b"\0\0\0\x18ftypM4A ".to_vec(), AudioFormat::Mp4);
    assert!(unmeasured.validate().is_ok());
    assert_eq!(
        unmeasured.admission().unit_ceilings.input_audio_seconds_ms,
        None
    );

    // Characters are Unicode scalar values, not bytes or UTF-16 units.
    let s = speech("h\u{e9}llo \u{1F44B}");
    assert_eq!(s.input_characters(), 7);
    assert!(s.validate().is_ok());
    let a = s.admission();
    assert_eq!(a.unit_ceilings.input_characters, Some(7));
    assert_eq!(a.unit_ceilings.output_audio_seconds_ms, None);
    assert_eq!(a.unit_ceilings.requests, Some(1));
    assert_eq!(s.usage().meters.unwrap().output_audio_seconds_ms, None);
    for bad in [
        SpeechRequest {
            input: "  ".into(),
            ..s.clone()
        },
        SpeechRequest {
            voice: "../alloy".into(),
            ..s.clone()
        },
        SpeechRequest {
            voice: String::new(),
            ..s.clone()
        },
        SpeechRequest {
            speed: Some(4.5),
            ..s.clone()
        },
        SpeechRequest {
            input: "x".repeat(SPEECH_HARD_MAX_INPUT_CHARS + 1),
            ..s.clone()
        },
    ] {
        assert_eq!(bad.validate(), Err(InferenceError::InvalidRequest));
    }
}

#[test]
fn audio_limits_are_configurable_and_bounded() {
    let limits = AudioLimits::from_lookup(|name| match name {
        "GATEWAY_AUDIO_MAX_UPLOAD_BYTES" => Some("1048576".into()),
        "GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS" => Some("12".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(limits.max_upload_bytes, 1024 * 1024);
    assert_eq!(limits.max_speech_input_chars, 12);
    assert_eq!(limits.max_speech_output_bytes, 64 * 1024 * 1024);
    assert_eq!(AudioLimits::default().max_upload_bytes, 25 * 1024 * 1024);
    for (name, bad) in [
        ("GATEWAY_AUDIO_MAX_UPLOAD_BYTES", "12"),
        ("GATEWAY_AUDIO_MAX_UPLOAD_BYTES", "x"),
        ("GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS", "0"),
        ("GATEWAY_AUDIO_SPEECH_MAX_OUTPUT_BYTES", "99999999999999"),
    ] {
        assert!(AudioLimits::from_lookup(|n| (n == name).then(|| bad.to_owned())).is_err());
    }
}

// ---------------------------------------------------------------- engine ----

#[derive(Default)]
struct Repo {
    deployments: Mutex<Vec<Deployment>>,
    admissions: Mutex<Vec<(bool, WorkloadAdmission)>>,
    finishes: Mutex<Vec<ExecutionFinish>>,
    finished: tokio::sync::Notify,
}
#[async_trait]
impl InferenceRepository for Repo {
    async fn deployments(&self, _: &Principal, _: &str) -> Result<Vec<Deployment>> {
        Ok(self.deployments.lock().unwrap().clone())
    }
    async fn start(&self, _: &ExecutionStart) -> Result<()> {
        panic!("audio must use admit_workload")
    }
    async fn admit_workload(
        &self,
        record: &ExecutionStart,
        admission: &WorkloadAdmission,
        _: i64,
        _: &Deployment,
    ) -> Result<()> {
        self.admissions
            .lock()
            .unwrap()
            .push((record.streamed, *admission));
        Ok(())
    }
    async fn finish(&self, record: &ExecutionFinish) -> Result<()> {
        self.finishes.lock().unwrap().push(ExecutionFinish {
            id: record.id,
            outcome: record.outcome,
            error: record.error,
            usage: record.usage,
            elapsed_ms: record.elapsed_ms,
        });
        self.finished.notify_one();
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Body {
    Chunks,
    Pending,
    Huge,
    Empty,
    FailMidway,
    WrongType,
}
struct Fake {
    body: Body,
    seconds_ms: Option<u64>,
    calls: AtomicUsize,
    dropped: Arc<AtomicBool>,
}
struct DropSignal(Arc<AtomicBool>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, p: ApiProtocol) -> bool {
        matches!(
            p,
            ApiProtocol::AudioTranscriptions | ApiProtocol::AudioSpeech
        )
    }
    fn supports_transcription_request(&self, _: &Deployment, r: &TranscriptionRequest) -> bool {
        r.prompt.is_none()
    }
    fn supports_speech_request(&self, _: &Deployment, r: &SpeechRequest) -> bool {
        r.response_format != SpeechFormat::Opus
    }
    async fn execute(&self, _: &Deployment, _: ChatRequest) -> Result<ProviderOutput> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_audio_transcription(
        &self,
        _: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.validate().is_ok());
        Ok(TranscriptionResponse {
            text: "hello".into(),
            usage: Usage {
                meters: Some(transcription_meters(self.seconds_ms)),
                ..Usage::default()
            },
        })
    }
    async fn execute_audio_speech(
        &self,
        _: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let signal = DropSignal(self.dropped.clone());
        let body = self.body;
        let audio: AudioStream = Box::pin(async_stream::stream! {
            let _signal = signal;
            match body {
                Body::Chunks | Body::WrongType => {
                    yield Ok(Bytes::from_static(b"\xff\xf3a"));
                    yield Ok(Bytes::new());
                    yield Ok(Bytes::from_static(b"bc"));
                }
                Body::Pending => {
                    yield Ok(Bytes::from_static(b"\xff\xf3"));
                    std::future::pending::<()>().await;
                }
                Body::Huge => loop {
                    yield Ok(Bytes::from(vec![0u8; 64 * 1024]));
                },
                Body::Empty => {}
                Body::FailMidway => {
                    yield Ok(Bytes::from_static(b"\xff\xf3"));
                    yield Err(InferenceError::UpstreamUnavailable);
                }
            }
        });
        Ok(SpeechResponse {
            audio,
            content_type: if body == Body::WrongType {
                "audio/wav"
            } else {
                request.response_format.content_type()
            },
            usage: request.usage(),
        })
    }
}

fn principal() -> Principal {
    Principal {
        workspace_id: Uuid::new_v4(),
        key_id: Uuid::new_v4(),
        user_id: None,
    }
}
fn setup(
    body: Body,
    protocol: &str,
    timeout: Duration,
    audio: AudioLimits,
) -> (Engine, Arc<Repo>, Arc<Fake>) {
    let repo = Arc::new(Repo::default());
    repo.deployments.lock().unwrap().push(Deployment {
        id: Uuid::new_v4(),
        provider: "fake".into(),
        upstream_model: "m".into(),
        credential_ref: "env:KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![protocol.into()],
    });
    let fake = Arc::new(Fake {
        body,
        seconds_ms: Some(1000),
        calls: AtomicUsize::new(0),
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let mut registry = ProviderRegistry::default();
    registry.register(fake.clone()).unwrap();
    let engine = Engine::new(
        repo.clone(),
        registry,
        EngineLimits {
            request_timeout: timeout,
            audio,
            ..EngineLimits::default()
        },
    )
    .unwrap();
    (engine, repo, fake)
}
const LONG: Duration = Duration::from_secs(30);
async fn collect(mut audio: AudioStream) -> (Vec<u8>, Option<InferenceError>) {
    let mut out = Vec::new();
    while let Some(item) = audio.next().await {
        match item {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

#[tokio::test]
async fn transcription_admits_measured_ceiling_and_settles_reported_seconds() {
    let (engine, repo, fake) = setup(
        Body::Chunks,
        "audio_transcriptions",
        LONG,
        AudioLimits::default(),
    );
    let id = Uuid::new_v4();
    let response = engine
        .execute_transcription(principal(), transcription(wav(850), AudioFormat::Wav), id)
        .await
        .unwrap();
    assert_eq!(response.text, "hello");
    let (streamed, admission) = repo.admissions.lock().unwrap()[0];
    assert!(!streamed);
    assert_eq!(admission.kind, WorkloadKind::AudioTranscriptions);
    assert_eq!(admission.unit_ceilings.input_audio_seconds_ms, Some(2000));
    let finish = &repo.finishes.lock().unwrap()[0];
    assert_eq!((finish.id, finish.outcome), (id, Outcome::Succeeded));
    assert_eq!(
        finish.usage.meters.unwrap().input_audio_seconds_ms,
        Some(1000)
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transcription_above_measured_ceiling_is_withheld_but_recorded() {
    let (engine, repo, _) = setup(
        Body::Chunks,
        "audio_transcriptions",
        LONG,
        AudioLimits::default(),
    );
    let fake = Fake {
        body: Body::Chunks,
        seconds_ms: Some(60_000),
        calls: AtomicUsize::new(0),
        dropped: Arc::new(AtomicBool::new(false)),
    };
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake)).unwrap();
    let engine = Engine::new(repo.clone(), registry, engine.limits()).unwrap();
    assert_eq!(
        engine
            .execute_transcription(
                principal(),
                transcription(wav(850), AudioFormat::Wav),
                Uuid::new_v4()
            )
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
    let finish = &repo.finishes.lock().unwrap()[0];
    assert_eq!(finish.outcome, Outcome::Failed);
    assert_eq!(
        finish.usage.meters.unwrap().input_audio_seconds_ms,
        Some(60_000)
    );
}

#[tokio::test]
async fn unsupported_or_oversize_transcriptions_never_admit() {
    let limits = AudioLimits {
        max_upload_bytes: 4096,
        ..AudioLimits::default()
    };
    let (engine, repo, fake) = setup(Body::Chunks, "audio_transcriptions", LONG, limits);
    let mut prompted = transcription(wav(100), AudioFormat::Wav);
    prompted.prompt = Some("Names: Ada".into());
    assert_eq!(
        engine
            .execute_transcription(principal(), prompted, Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
    assert_eq!(
        engine
            .execute_transcription(
                principal(),
                transcription(wav(1000), AudioFormat::Wav),
                Uuid::new_v4()
            )
            .await
            .err(),
        Some(InferenceError::InvalidRequest)
    );
    // A speech model is never selected for transcription.
    let (engine2, repo2, fake2) = setup(Body::Chunks, "audio_speech", LONG, limits);
    assert_eq!(
        engine2
            .execute_transcription(
                principal(),
                transcription(wav(100), AudioFormat::Wav),
                Uuid::new_v4()
            )
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
    for (repo, fake) in [(repo, fake), (repo2, fake2)] {
        assert!(repo.admissions.lock().unwrap().is_empty());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn speech_streams_and_settles_only_after_upstream_eof() {
    let (engine, repo, _) = setup(Body::Chunks, "audio_speech", LONG, AudioLimits::default());
    let id = Uuid::new_v4();
    let response = engine
        .execute_speech(principal(), speech("Hi \u{1F44B}"), id)
        .await
        .unwrap();
    assert_eq!(response.content_type, "audio/mpeg");
    let (streamed, admission) = repo.admissions.lock().unwrap()[0];
    assert!(streamed);
    assert_eq!(admission.kind, WorkloadKind::AudioSpeech);
    assert_eq!(admission.unit_ceilings.input_characters, Some(4));
    // Not settled while the body is still open.
    assert!(repo.finishes.lock().unwrap().is_empty());
    let (bytes, error) = collect(response.audio).await;
    assert_eq!((bytes.as_slice(), error), (&b"\xff\xf3abc"[..], None));
    let finish = &repo.finishes.lock().unwrap()[0];
    assert_eq!((finish.id, finish.outcome), (id, Outcome::Succeeded));
    assert_eq!(finish.usage.meters.unwrap().input_characters, Some(4));
    assert_eq!(finish.usage.meters.unwrap().output_audio_seconds_ms, None);
}

#[tokio::test]
async fn dropping_speech_body_cancels_upstream_and_keeps_usage() {
    let (engine, repo, fake) = setup(Body::Pending, "audio_speech", LONG, AudioLimits::default());
    let mut response = engine
        .execute_speech(principal(), speech("Hello"), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        &response.audio.next().await.unwrap().unwrap()[..],
        b"\xff\xf3"
    );
    assert!(!fake.dropped.load(Ordering::SeqCst));
    drop(response);
    assert!(fake.dropped.load(Ordering::SeqCst), "upstream not dropped");
    tokio::time::timeout(Duration::from_secs(3), repo.finished.notified())
        .await
        .unwrap();
    let finish = &repo.finishes.lock().unwrap()[0];
    assert_eq!(finish.outcome, Outcome::Cancelled);
    assert_eq!(finish.usage.meters.unwrap().input_characters, Some(5));
}

#[tokio::test]
async fn speech_failures_abort_the_body_and_are_accounted() {
    let small = AudioLimits {
        max_speech_output_bytes: 64 * 1024,
        ..AudioLimits::default()
    };
    for (body, timeout, expected) in [
        (Body::Huge, LONG, InferenceError::InvalidUpstream),
        (Body::FailMidway, LONG, InferenceError::UpstreamUnavailable),
        (
            Body::Pending,
            Duration::from_millis(50),
            InferenceError::Timeout,
        ),
    ] {
        let (engine, repo, fake) = setup(body, "audio_speech", timeout, small);
        let response = engine
            .execute_speech(principal(), speech("Hello"), Uuid::new_v4())
            .await
            .unwrap();
        let (_, error) = collect(response.audio).await;
        assert_eq!(error, Some(expected));
        let finish = &repo.finishes.lock().unwrap()[0];
        assert_eq!(
            (finish.outcome, finish.error),
            (Outcome::Failed, Some(expected))
        );
        assert!(fake.dropped.load(Ordering::SeqCst));
    }
    // Empty bodies and mismatched media types never return success.
    let (engine, repo, _) = setup(Body::Empty, "audio_speech", LONG, small);
    let response = engine
        .execute_speech(principal(), speech("Hello"), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        collect(response.audio).await,
        (vec![], Some(InferenceError::InvalidUpstream))
    );
    assert_eq!(repo.finishes.lock().unwrap()[0].outcome, Outcome::Failed);
    let (engine, repo, _) = setup(Body::WrongType, "audio_speech", LONG, small);
    assert_eq!(
        engine
            .execute_speech(principal(), speech("Hello"), Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
    assert_eq!(repo.finishes.lock().unwrap()[0].outcome, Outcome::Failed);
}

#[tokio::test]
async fn speech_bounds_and_capabilities_fail_before_admission() {
    let limits = AudioLimits {
        max_speech_input_chars: 12,
        ..AudioLimits::default()
    };
    let (engine, repo, fake) = setup(Body::Chunks, "audio_speech", LONG, limits);
    // 13 scalar values (multibyte included) exceeds the 12-character cap.
    assert_eq!(
        engine
            .execute_speech(
                principal(),
                speech("\u{e9}".repeat(13).as_str()),
                Uuid::new_v4()
            )
            .await
            .err(),
        Some(InferenceError::InvalidRequest)
    );
    let mut opus = speech("Hello");
    opus.response_format = SpeechFormat::Opus;
    assert_eq!(
        engine
            .execute_speech(principal(), opus, Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
    assert!(repo.admissions.lock().unwrap().is_empty());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    // Exactly at the cap is accepted.
    assert!(
        engine
            .execute_speech(
                principal(),
                speech("\u{e9}".repeat(12).as_str()),
                Uuid::new_v4()
            )
            .await
            .is_ok()
    );
}
