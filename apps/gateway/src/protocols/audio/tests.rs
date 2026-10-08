use super::*;
use crate::inference::{audio::tests_support::wav, types::Usage};

const BOUNDARY: &str = "----form7MA4YWxk";

fn headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&format!("multipart/form-data; boundary={BOUNDARY}")).unwrap(),
    );
    h
}
/// `(name, filename, content type, data)` parts.
fn form(parts: &[P]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, filename, content_type, data) in parts {
        out.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"").as_bytes(),
        );
        if let Some(f) = filename {
            out.extend_from_slice(format!("; filename=\"{f}\"").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        if let Some(t) = content_type {
            out.extend_from_slice(format!("Content-Type: {t}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}
fn parse(body: &[u8]) -> Result<TranscriptionRequest, Option<InferenceError>> {
    parse_transcription(&headers(), body, 64 * 1024).map_err(|r| match r {
        Rejection::Error(e) => Some(e),
        Rejection::TooLarge => None,
    })
}

#[test]
fn transcription_form_happy_path_never_keeps_the_client_filename() {
    let audio: &'static [u8] = Box::leak(wav(850).into_boxed_slice());
    let body = form(&[
        field("model", "company/whisper"),
        file("../../etc/secret name.WAV", Some("audio/x-wav"), audio),
        field("language", "en"),
        field("prompt", "Glossary: Ada"),
        field("response_format", "text"),
        field("temperature", "0.2"),
        field("stream", "false"),
    ]);
    let r = parse(&body).ok().unwrap();
    assert_eq!(r.model, "company/whisper");
    assert_eq!(r.format, AudioFormat::Wav);
    assert_eq!(&r.audio[..], audio);
    assert_eq!(r.duration_ms, Some(850));
    assert_eq!(r.response_format, TranscriptionFormat::Text);
    assert_eq!(
        (r.language.as_deref(), r.temperature),
        (Some("en"), Some(0.2))
    );
    // Generic octet-stream defers to the extension; no content type too.
    for ct in [Some("application/octet-stream"), None] {
        let body = form(&[file("a.wav", ct, audio), field("model", "m")]);
        assert_eq!(
            parse(&body).ok().unwrap().response_format,
            TranscriptionFormat::Json
        );
    }
}

type P = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    &'static [u8],
);
fn field(name: &'static str, value: &'static str) -> P {
    (name, None, None, value.as_bytes())
}
fn file(name: &'static str, ct: Option<&'static str>, data: &'static [u8]) -> P {
    ("file", Some(name), ct, data)
}

#[test]
fn transcription_form_edge_cases() {
    let audio: &'static [u8] = Box::leak(wav(100).into_boxed_slice());
    let model = field("model", "m");
    let ok = file("a.wav", None, audio);
    let invalid = Some(InferenceError::InvalidRequest);
    let unsupported = Some(InferenceError::Unsupported);
    let cases: Vec<(Vec<P>, Option<InferenceError>)> = vec![
        // Missing file / model, duplicate file, file without filename.
        (vec![model], invalid),
        (vec![ok], invalid),
        (vec![model, ok, file("b.wav", None, audio)], invalid),
        (vec![model, ("file", None, None, audio)], invalid),
        // Disallowed extension/type, mismatched type, wrong magic, empty file.
        (vec![model, file("a.exe", None, audio)], invalid),
        (vec![model, file("noext", None, audio)], invalid),
        (
            vec![model, file("a.wav", Some("text/plain"), audio)],
            invalid,
        ),
        (
            vec![model, file("a.wav", Some("audio/mpeg"), audio)],
            invalid,
        ),
        (vec![model, file("a.mp3", None, audio)], invalid),
        (vec![model, file("a.wav", None, b"")], invalid),
        // Text field with a filename, duplicate field, unknown or bad fields.
        (
            vec![model, ok, ("language", Some("x.txt"), None, b"en")],
            invalid,
        ),
        (vec![model, model, ok], invalid),
        (vec![model, ok, field("user", "x")], invalid),
        (vec![model, ok, field("temperature", "hot")], invalid),
        (vec![model, ok, field("temperature", "2")], invalid),
        (vec![model, ok, field("response_format", "xml")], invalid),
        (
            vec![model, ok, ("prompt", None, None, &[0xff, 0xfe])],
            invalid,
        ),
        // Recognized OpenAI options outside the subset.
        (
            vec![model, ok, field("response_format", "verbose_json")],
            unsupported,
        ),
        (
            vec![model, ok, field("response_format", "srt")],
            unsupported,
        ),
        (
            vec![model, ok, field("timestamp_granularities[]", "word")],
            unsupported,
        ),
        (vec![model, ok, field("stream", "true")], unsupported),
        (vec![model, ok, field("include[]", "logprobs")], unsupported),
    ];
    for (i, (parts, expected)) in cases.into_iter().enumerate() {
        assert_eq!(parse(&form(&parts)).err(), Some(expected), "case {i}");
    }
    // Oversize field and oversize file (413 rather than 400).
    let long: &'static str = Box::leak("a".repeat(MAX_FIELD_BYTES + 1).into_boxed_str());
    assert_eq!(
        parse(&form(&[model, ok, field("prompt", long)])).err(),
        Some(invalid)
    );
    let big: &'static [u8] = Box::leak(wav(3000).into_boxed_slice());
    assert!(big.len() > 64 * 1024);
    assert_eq!(
        parse(&form(&[model, file("a.wav", None, big)])).err(),
        Some(None)
    );
    // Wrong or missing request content type.
    let mut json = HeaderMap::new();
    json.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    assert!(parse_transcription(&json, b"{}", 1024).is_err());
    assert!(parse_transcription(&HeaderMap::new(), b"", 1024).is_err());
    // Too many parts.
    let many: Vec<P> = (0..MAX_PARTS + 1).map(|_| field("x", "y")).collect();
    assert_eq!(parse(&form(&many)).err(), Some(invalid));
    assert_eq!(extension("dir/a.b.FLAC"), Some("flac".into()));
    assert_eq!(extension("C:\\x\\clip.mp3"), Some("mp3".into()));
    assert_eq!(extension("clip."), None);
}

#[test]
fn transcription_usage_renders_observed_counters_only() {
    let mut response = TranscriptionResponse {
        text: String::new(),
        usage: Usage::default(),
    };
    assert_eq!(transcription_usage(&response), None);
    response.usage.meters = Some(crate::inference::audio::transcription_meters(Some(9201)));
    assert_eq!(
        transcription_usage(&response),
        Some(json!({"type":"duration","seconds":9.201}))
    );
    response.usage.input_tokens = Some(8);
    response.usage.output_tokens = Some(6);
    assert_eq!(
        transcription_usage(&response),
        Some(json!({"type":"tokens","input_tokens":8,"output_tokens":6,"total_tokens":14}))
    );
}

fn speech(v: Value) -> Result<SpeechRequest, InferenceError> {
    serde_json::from_value::<SpeechWire>(v)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}

#[test]
fn speech_request_shape() {
    let r = speech(json!({"model":"m","input":"Hi","voice":"alloy"})).unwrap();
    assert_eq!((r.response_format, r.speed), (SpeechFormat::Mp3, None));
    let r = speech(json!({"model":"m","input":"Hi","voice":"alloy","response_format":"pcm","speed":1.5,"stream_format":"audio"})).unwrap();
    assert_eq!((r.response_format, r.speed), (SpeechFormat::Pcm, Some(1.5)));
    for (bad, expected) in [
        (
            json!({"model":"m","input":"Hi"}),
            InferenceError::InvalidRequest,
        ),
        (
            json!({"model":"m","input":"","voice":"alloy"}),
            InferenceError::InvalidRequest,
        ),
        (
            json!({"model":"m","input":"Hi","voice":"alloy","response_format":"ogg"}),
            InferenceError::InvalidRequest,
        ),
        (
            json!({"model":"m","input":"Hi","voice":"alloy","speed":0.1}),
            InferenceError::InvalidRequest,
        ),
        (
            json!({"model":"m","input":"Hi","voice":"alloy","provider":{"only":["x"]}}),
            InferenceError::InvalidRequest,
        ),
        (
            json!({"model":"m","input":"Hi","voice":{"id":"x"}}),
            InferenceError::InvalidRequest,
        ),
        (
            json!({"model":"m","input":"Hi","voice":"alloy","response_format":"aac"}),
            InferenceError::Unsupported,
        ),
        (
            json!({"model":"m","input":"Hi","voice":"alloy","instructions":"whisper"}),
            InferenceError::Unsupported,
        ),
        (
            json!({"model":"m","input":"Hi","voice":"alloy","stream_format":"sse"}),
            InferenceError::Unsupported,
        ),
    ] {
        assert_eq!(speech(bad.clone()).err(), Some(expected), "{bad}");
    }
}
